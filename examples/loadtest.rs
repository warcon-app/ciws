//! Load test: fake game servers post kill-feed batches through an in-process relay, which sends
//! each one to every sink. Reports how fast the game gets its answer and how far behind the
//! fan-out ever gets. The relay, the games and the sinks share one machine and one runtime, so
//! the numbers are a floor, not a ceiling.
//!
//! A full 100-player server posts about 550 times an hour (measured on a busy community server), so 0.15 posts a
//! second; the default here is three times that. The relay lets a feed post up to 20 times a
//! second, so heavier loads take more servers, not faster ones.
//!
//! Every community server there is, at three times full population:
//!   cargo run --release --example loadtest -- --feeds 1500 --rate 0.5 --seconds 30
//! Far past anything real:
//!   cargo run --release --example loadtest -- --feeds 2000 --rate 15 --seconds 10

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::routing::post;
use ciws::{Config, Relay};
use clap::Parser;
use tokio::net::TcpListener;

#[derive(Parser)]
struct Args {
    /// Game servers, one feed each.
    #[arg(long, default_value_t = 1500)]
    feeds: usize,
    /// Targets per feed (each on its own sink).
    #[arg(long, default_value_t = 3)]
    targets: usize,
    #[arg(long, default_value_t = 20)]
    seconds: u64,
    /// Posts per second per game server (at most 20).
    #[arg(long, default_value_t = 0.5)]
    rate: f64,
    /// Kill events per batch (the game sends one to ten, 1.3 on average).
    #[arg(long, default_value_t = 1)]
    events: usize,
    /// How long each sink takes to answer, in milliseconds.
    #[arg(long, default_value_t = 0)]
    sink_delay_ms: u64,
    /// Run this ciws binary as its own process instead of a relay inside the test, and report
    /// its own CPU time and memory.
    #[arg(long)]
    relay_bin: Option<std::path::PathBuf>,
}

/// Where the relay runs: inside this process, or as its own.
enum Under {
    Here(Relay),
    Child(std::process::Child),
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// (resident memory in MB, CPU seconds) of a process, from ps.
fn usage(pid: u32) -> Option<(f64, f64)> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=,time=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let mut parts = text.split_whitespace();
    let rss_mb = parts.next()?.parse::<f64>().ok()? / 1024.0;
    // [[dd-]hh:]mm:ss[.ss]
    let cpu = parts
        .next()?
        .split(['-', ':'])
        .fold(0.0, |acc, p| acc * 60.0 + p.parse::<f64>().unwrap_or(0.0));
    Some((rss_mb, cpu))
}

#[derive(Default)]
struct Sink {
    requests: AtomicU64,
    delay_ms: u64,
}

async fn sink_answer(State(s): State<Arc<Sink>>, _body: Bytes) -> &'static str {
    if s.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(s.delay_ms)).await;
    }
    s.requests.fetch_add(1, Relaxed);
    r#"{"ok":true}"#
}

fn batch(events: usize, n: usize) -> String {
    let one = |i: usize| {
        format!(
            r#"{{"eventId":"{n:08x}-{i:04x}-4000-8000-000000000000","type":"killed","eventTime":3317.77,"matchId":"6f1c2d8e-4b7a-4e21-9c3f-0a5b6c7d8e9f","mapName":"Kavkazi","killerName":"[ABC] Night Owl","killerId":"-ab0d-3ff278214bfe","killerSteamId":"76561198100000101","victimName":"Ghostpepper","victimId":"-cd12-8a9b0c1d2e3f","victimSteamId":"76561198100000102","cause":"Id.Item.AK74M","distance":4012.5,"contextTags":["Meta.Progression.Context.Player.KillContext.Headshot","Meta.PlayerKillFlag.Player.Local.Kill","Meta.PlayerKillFlag.Player.Local.Death"]}}"#
        )
    };
    let events: Vec<String> = (0..events).map(one).collect();
    format!(
        r#"{{"serverId":"e9cf2544-1b2c-4d5e-8f90-a1b2c3d4e5f6","serverName":"[ABC] Example Server | Vanilla #{n}","events":[{}]}}"#,
        events.join(",")
    )
}

fn token(i: usize) -> String {
    format!("cws_loadtest_feed_{i:06}_0123456789abcdef")
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    ciws::deliver::use_ring();
    assert!(
        args.rate > 0.0 && args.rate <= 20.0,
        "--rate is 0 to 20 posts a second"
    );

    let mut sinks = Vec::new();
    let mut sink_urls = Vec::new();
    for _ in 0..args.targets {
        let sink = Arc::new(Sink {
            delay_ms: args.sink_delay_ms,
            ..Sink::default()
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        sink_urls.push(format!("http://{}", listener.local_addr().unwrap()));
        let app = Router::new()
            .fallback(post(sink_answer))
            .with_state(sink.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        sinks.push(sink);
    }

    let (port, admin_port) = if args.relay_bin.is_some() {
        (free_port(), free_port())
    } else {
        (0, 0)
    };
    let mut toml =
        format!("listen = \"127.0.0.1:{port}\"\nadmin_listen = \"127.0.0.1:{admin_port}\"\n");
    for i in 0..args.feeds {
        toml.push_str(&format!(
            "[[feed]]\nname = \"server-{i}\"\ntoken = \"{}\"\n",
            token(i)
        ));
        for url in &sink_urls {
            toml.push_str(&format!(
                "[[feed.target]]\nurl = \"{url}\"\ntoken = \"wkf_{i}\"\n"
            ));
        }
    }
    let config_file = tempfile::NamedTempFile::new().unwrap();
    let (under, ingest) = match &args.relay_bin {
        None => {
            let relay = Relay::start(Config::parse(&toml).unwrap(), None)
                .await
                .unwrap();
            let addr = relay.ingest_addr;
            (Under::Here(relay), addr)
        }
        Some(bin) => {
            std::fs::write(config_file.path(), &toml).unwrap();
            let child = std::process::Command::new(bin)
                .args(["run", "-c"])
                .arg(config_file.path())
                .env("RUST_LOG", "warn")
                .spawn()
                .expect("cannot start the relay binary");
            let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            let health = format!("http://{addr}/healthz");
            let up = Instant::now();
            while reqwest::get(&health).await.is_err() {
                assert!(
                    up.elapsed() < Duration::from_secs(30),
                    "the relay did not start"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            (Under::Child(child), addr)
        }
    };
    let url = format!("http://{ingest}/api/ingest/events");
    println!(
        "{} game servers x {} targets ({} lanes), {} posts/s each ({:.0} in all), {} event(s) ({} bytes) per batch, {} s",
        args.feeds,
        args.targets,
        args.feeds * args.targets,
        args.rate,
        args.rate * args.feeds as f64,
        args.events,
        batch(args.events, 0).len(),
        args.seconds
    );

    // Sample how far behind the fan-out is while the games post, and the relay's memory.
    let accepted = Arc::new(AtomicU64::new(0));
    let behind_peak = Arc::new(AtomicU64::new(0));
    let rss_peak_kb = Arc::new(AtomicU64::new(0));
    let child_pid = match &under {
        Under::Child(c) => Some(c.id()),
        Under::Here(_) => None,
    };
    let sampler = {
        let (sinks, behind_peak, accepted, rss_peak_kb) = (
            sinks.clone(),
            behind_peak.clone(),
            accepted.clone(),
            rss_peak_kb.clone(),
        );
        let targets = args.targets as u64;
        tokio::spawn(async move {
            let mut n = 0u64;
            loop {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let delivered: u64 = sinks.iter().map(|s| s.requests.load(Relaxed)).sum();
                behind_peak.fetch_max(
                    (accepted.load(Relaxed) * targets).saturating_sub(delivered),
                    Relaxed,
                );
                n += 1;
                if let (Some(pid), true) = (child_pid, n % 25 == 0) {
                    if let Some((mb, _)) = usage(pid) {
                        rss_peak_kb.fetch_max((mb * 1024.0) as u64, Relaxed);
                    }
                }
            }
        })
    };

    let failed = Arc::new(AtomicU64::new(0));
    let kinds = Arc::new(Mutex::new(std::collections::BTreeMap::<String, u64>::new()));
    let latencies = Arc::new(Mutex::new(Vec::<u32>::new()));
    let started = Instant::now();
    let deadline = started + Duration::from_secs(args.seconds);
    let interval = Duration::from_secs_f64(1.0 / args.rate);
    let mut games = Vec::new();
    for i in 0..args.feeds {
        let (url, failed, latencies, kinds, accepted) = (
            url.clone(),
            failed.clone(),
            latencies.clone(),
            kinds.clone(),
            accepted.clone(),
        );
        // Each game server is its own process with its own connection, as in the real world.
        let client = reqwest::Client::new();
        let body = Bytes::from(batch(args.events, i));
        let auth = format!("Bearer {}", token(i));
        let feeds = args.feeds;
        games.push(tokio::spawn(async move {
            let mut mine = Vec::new();
            // Spread the servers' first posts over two seconds (or one interval, if longer), as
            // real ones would be: thousands of connections opened at once on one machine
            // overflow its listen queue (128 on macOS), the test's limit and not the relay's.
            let spread = interval.max(Duration::from_secs(2));
            let mut next = started + spread.mul_f64(i as f64 / feeds as f64);
            loop {
                tokio::time::sleep_until(next.into()).await;
                next += interval;
                if Instant::now() >= deadline {
                    break;
                }
                let t = Instant::now();
                let r = client
                    .post(&url)
                    .header("authorization", &auth)
                    .header(
                        "user-agent",
                        "Wardogs/++Wardogs+Live-CL-507060 (http-eventloop) Linux/debian12",
                    )
                    .header("content-type", "application/json")
                    .body(body.clone())
                    .send()
                    .await;
                match r {
                    Ok(resp) if resp.status() == 200 => {
                        let _ = resp.bytes().await;
                        mine.push(t.elapsed().as_micros() as u32);
                        accepted.fetch_add(1, Relaxed);
                    }
                    Ok(resp) => {
                        failed.fetch_add(1, Relaxed);
                        let kind = format!("answered {}", resp.status().as_u16());
                        *kinds.lock().unwrap().entry(kind).or_insert(0) += 1;
                    }
                    Err(e) => {
                        failed.fetch_add(1, Relaxed);
                        let kind = if e.is_timeout() {
                            "timed out"
                        } else if e.is_connect() {
                            "could not connect"
                        } else {
                            "request failed"
                        };
                        *kinds.lock().unwrap().entry(kind.to_owned()).or_insert(0) += 1;
                    }
                }
            }
            latencies.lock().unwrap().extend(mine);
        }));
    }
    for g in games {
        g.await.unwrap();
    }
    let posting = started.elapsed();
    let posts = accepted.load(Relaxed);
    let expected = posts * args.targets as u64;
    let drained_at = loop {
        let delivered: u64 = sinks.iter().map(|s| s.requests.load(Relaxed)).sum();
        if delivered >= expected {
            break Instant::now();
        }
        if started.elapsed() > posting + Duration::from_secs(120) {
            println!("gave up waiting: {delivered} of {expected} deliveries");
            break Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    sampler.abort();

    let mut lat = std::mem::take(&mut *latencies.lock().unwrap());
    lat.sort_unstable();
    let pct = |p: f64| -> f64 {
        if lat.is_empty() {
            return 0.0;
        }
        lat[((lat.len() - 1) as f64 * p).round() as usize] as f64 / 1000.0
    };
    let secs = posting.as_secs_f64();
    let delivered: u64 = sinks.iter().map(|s| s.requests.load(Relaxed)).sum();
    println!();
    println!(
        "ingest     {posts} batches accepted, {} failed, {:.0} batches/s, {:.0} kill events/s",
        failed.load(Relaxed),
        posts as f64 / secs,
        (posts * args.events as u64) as f64 / secs
    );
    if !kinds.lock().unwrap().is_empty() {
        println!("failures   {:?}", kinds.lock().unwrap());
    }
    println!(
        "answer     p50 {:.3} ms  p90 {:.3} ms  p99 {:.3} ms  max {:.2} ms (game post to its 200)",
        pct(0.5),
        pct(0.9),
        pct(0.99),
        pct(1.0)
    );
    println!(
        "fan-out    {delivered} deliveries, {:.0}/s; at most {} behind while posting; all done {:.3} s after the last post",
        delivered as f64 / drained_at.duration_since(started).as_secs_f64(),
        behind_peak.load(Relaxed),
        drained_at.duration_since(started + posting).as_secs_f64()
    );
    match under {
        Under::Here(relay) => relay.shutdown().await,
        Under::Child(mut child) => {
            if let Some((rss_mb, cpu_s)) = usage(child.id()) {
                let peak = (rss_peak_kb.load(Relaxed) as f64 / 1024.0).max(rss_mb);
                println!(
                    "relay      its own process: {cpu_s:.1} s of CPU in {:.1} s ({:.0}% of one core), {peak:.0} MB at most",
                    started.elapsed().as_secs_f64(),
                    cpu_s / started.elapsed().as_secs_f64() * 100.0
                );
            }
            let _ = std::process::Command::new("kill")
                .arg(child.id().to_string())
                .status();
            let _ = child.wait();
        }
    }
}
