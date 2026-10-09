//! Starting, reloading and stopping a relay: the routing table, the lanes, the two listeners, and
//! the watch on `ciws.toml` that applies an edit as soon as it is saved.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use axum::Router;
use axum::serve::ListenerExt;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::lanes::{LaneCtx, Supervisor};
use crate::metrics::Metrics;
use crate::routes::{Routes, feed_label};
use crate::{admin, deliver, ingest, now_ms};

/// How long a stopping relay keeps delivering what is still queued.
const DRAIN: Duration = Duration::from_secs(5);
/// How often `ciws.toml` is looked at for an edit.
const WATCH_EVERY: Duration = Duration::from_secs(2);

/// What the ingest route needs.
pub struct App {
    pub routes: ArcSwap<Routes>,
    pub metrics: Metrics,
    pub started_ms: u64,
}

/// What the admin listener and a reload need on top.
pub struct Control {
    pub app: Arc<App>,
    supervisor: Supervisor,
    config_path: Option<PathBuf>,
    fixed: Fixed,
    reload_lock: tokio::sync::Mutex<()>,
}

/// The settings a reload cannot change; they take a restart.
#[derive(Debug, PartialEq)]
struct Fixed {
    listen: SocketAddr,
    admin_listen: SocketAddr,
    block_private: bool,
}

impl Fixed {
    fn of(c: &Config) -> Fixed {
        Fixed {
            listen: c.listen,
            admin_listen: c.admin_listen,
            block_private: c.delivery.block_private,
        }
    }
}

pub struct Relay {
    /// Where the game posts (useful when `listen` asked for port 0).
    pub ingest_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    control: Arc<Control>,
    stop_listening: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl Relay {
    /// Starts listening and delivering. With a `config_path`, an edit to that file is applied
    /// as soon as it is saved.
    pub async fn start(config: Config, config_path: Option<PathBuf>) -> Result<Relay> {
        let client = deliver::client(&config.delivery)?;
        let ingest_listener = TcpListener::bind(config.listen).await.with_context(|| {
            format!(
                "cannot listen on {} (is CIWS already running?)",
                config.listen
            )
        })?;
        let admin_listener = TcpListener::bind(config.admin_listen)
            .await
            .with_context(|| format!("cannot listen on {} (admin_listen)", config.admin_listen))?;
        let ingest_addr = ingest_listener.local_addr()?;
        let admin_addr = admin_listener.local_addr()?;

        let app = Arc::new(App {
            routes: ArcSwap::from_pointee(Routes::build(&config, None)),
            metrics: Metrics::default(),
            started_ms: now_ms(),
        });
        let supervisor = Supervisor::new(LaneCtx::new(client));
        supervisor.reconcile(&app.routes.load());
        let control = Arc::new(Control {
            app: app.clone(),
            supervisor,
            config_path: config_path.clone(),
            fixed: Fixed::of(&config),
            reload_lock: tokio::sync::Mutex::new(()),
        });

        let stop_listening = CancellationToken::new();
        let mut tasks = vec![
            serve(
                ingest_listener,
                ingest::router(app),
                stop_listening.clone(),
                "ingest",
            ),
            serve(
                admin_listener,
                admin::router(control.clone()),
                stop_listening.clone(),
                "admin",
            ),
            tokio::spawn(watch_lanes(control.clone(), stop_listening.clone())),
        ];
        if let Some(path) = config_path {
            tasks.push(tokio::spawn(watch_file(
                control.clone(),
                path,
                stop_listening.clone(),
            )));
        }
        // Listening on every interface is not an address a browser opens; this machine's is.
        let mut status_at = admin_addr;
        if status_at.ip().is_unspecified() {
            status_at.set_ip(if status_at.is_ipv4() {
                std::net::Ipv4Addr::LOCALHOST.into()
            } else {
                std::net::Ipv6Addr::LOCALHOST.into()
            });
        }
        info!("listening for the game on {ingest_addr}; status at http://{status_at}/status");
        describe(&config);
        Ok(Relay {
            ingest_addr,
            admin_addr,
            control,
            stop_listening,
            tasks,
        })
    }

    pub fn app(&self) -> &Arc<App> {
        &self.control.app
    }

    /// Applies a configuration: new targets start, removed ones stop (and what was queued for
    /// them goes), changed ones start again with their queue.
    pub async fn reload(&self, config: Config) {
        self.control.reload(config).await;
    }

    pub async fn reload_from_file(&self) -> Result<()> {
        self.control.reload_from_file().await
    }

    /// Stops taking batches, lets the posts in flight finish, gives what is queued a few seconds
    /// to go out, then stops the lanes.
    pub async fn shutdown(mut self) {
        self.stop_listening.cancel();
        for task in self.tasks.drain(..) {
            let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
        }
        let deadline = Instant::now() + DRAIN;
        while self.control.waiting() > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let left = self.control.waiting();
        self.control.supervisor.stop_all().await;
        if left > 0 {
            warn!(
                batches = left,
                "stopped with batches its targets had not taken yet"
            );
        }
        info!("stopped");
    }
}

impl Control {
    pub async fn reload_from_file(&self) -> Result<()> {
        let path = self
            .config_path
            .as_deref()
            .context("this relay was not started from a file")?;
        let config = Config::load(path)?;
        self.reload(config).await;
        Ok(())
    }

    pub async fn reload(&self, config: Config) {
        let _guard = self.reload_lock.lock().await;
        if Fixed::of(&config) != self.fixed {
            warn!("listen, admin_listen and delivery.block_private change only when CIWS restarts");
        }
        let routes = Routes::build(&config, Some(&self.app.routes.load()));
        self.app.routes.store(Arc::new(routes));
        self.supervisor.reconcile(&self.app.routes.load());
        info!("settings changed");
        describe(&config);
    }

    /// Batches waiting across every target, counting those being delivered.
    fn waiting(&self) -> u64 {
        self.app
            .routes
            .load()
            .lanes
            .values()
            .map(|l| l.shared.backlog())
            .sum()
    }
}

/// Says where each feed goes, in the log: what someone setting CIWS up looks for.
fn describe(config: &Config) {
    if config.feeds.is_empty() {
        warn!("nothing to do yet: add a token and a [[target]] to ciws.toml");
    }
    for f in &config.feeds {
        let label = feed_label(&f.name);
        if f.targets.is_empty() {
            warn!("{label} has nowhere to go yet: add a [[target]] to ciws.toml");
        } else {
            let names: Vec<String> = f.targets.iter().map(|t| t.shown()).collect();
            info!("{label} goes to {}", names.join(", "));
        }
    }
}

fn serve(
    listener: TcpListener,
    router: Router,
    stop: CancellationToken,
    name: &'static str,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let listener = listener.tap_io(|tcp| {
            let _ = tcp.set_nodelay(true);
        });
        if let Err(e) = axum::serve(listener, router)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
        {
            error!("the {name} listener failed: {e}");
        }
    })
}

/// Restarts any lane whose task ended without being asked to.
async fn watch_lanes(control: Arc<Control>, stop: CancellationToken) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            () = stop.cancelled() => return,
        }
        if let Ok(_guard) = control.reload_lock.try_lock() {
            control.supervisor.reconcile(&control.app.routes.load());
        }
    }
}

/// Applies `ciws.toml` whenever it is saved. A file with a mistake is reported and the settings
/// already running stay.
async fn watch_file(control: Arc<Control>, path: PathBuf, stop: CancellationToken) {
    let mut seen = stamp(&path);
    let mut tick = tokio::time::interval(WATCH_EVERY);
    tick.tick().await;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            () = stop.cancelled() => return,
        }
        if stamp(&path) == seen {
            continue;
        }
        // Give the editor a moment to finish writing.
        tokio::time::sleep(Duration::from_millis(250)).await;
        seen = stamp(&path);
        match Config::load(&path) {
            Ok(config) => control.reload(config).await,
            Err(e) => {
                error!("{e:#}");
                warn!("the settings already running stay until that is fixed");
            }
        }
    }
}

fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}
