use std::fs::OpenOptions;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use ciws::config::{self, GAME_PATH};
use ciws::{Config, Relay};

/// musl's allocator, which the static Linux builds would otherwise use, falls far behind under
/// load; mimalloc keeps every build as fast as glibc's.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(
    name = "ciws",
    version,
    about = "Sends your WARDOGS server's kill feed to as many places as you like"
)]
struct Cli {
    /// The settings file.
    #[arg(
        short,
        long,
        env = "CIWS_CONFIG",
        default_value = "ciws.toml",
        global = true
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run CIWS (the default). Makes the settings file first if there is none.
    Run,
    /// Make the settings file with a new token, and say what to put in the game's config.
    Init,
    /// Check the settings file and show what it sets up, without its secrets.
    Check,
    /// Print a new token.
    Token,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Token => {
            println!("{}", ciws::token::mint());
            ExitCode::SUCCESS
        }
        Command::Init if !std::io::stdout().is_terminal() => {
            // `ciws init > ciws.toml`, as with Docker: the file goes to the shell, which writes it
            // as whoever ran the command, and the steps go to the screen.
            let token = ciws::token::mint();
            print!("{}", config::starter(&token));
            let _ = first_steps(&mut std::io::stderr(), None, &token);
            ExitCode::SUCCESS
        }
        Command::Init => match init(&cli.config) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => {
                eprintln!(
                    "{} is already there; it was left as it is.",
                    cli.config.display()
                );
                ExitCode::FAILURE
            }
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::FAILURE
            }
        },
        Command::Check => match Config::load(&cli.config) {
            Ok(config) => {
                print_summary(&config);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::FAILURE
            }
        },
        Command::Run => {
            if let Err(e) = init(&cli.config) {
                eprintln!("error: there is no settings file, and {e:#}");
                if Path::new("/.dockerenv").exists() {
                    eprintln!(
                        "Make it first, in the folder you mount:\n\n    \
                         docker run --rm ghcr.io/warcon-app/ciws init > ciws.toml\n"
                    );
                }
                return pause_then(ExitCode::FAILURE);
            }
            run(cli.config)
        }
    }
}

/// Writes a starter settings file with a new token, unless there is one. Answers whether it did.
fn init(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    let token = ciws::token::mint();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        // It holds the token: for its owner's eyes only.
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    file.write_all(config::starter(&token).as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))?;
    let shown = path.canonicalize().unwrap_or_else(|_| path.to_owned());
    let _ = first_steps(&mut std::io::stdout(), Some(&shown), &token);
    Ok(true)
}

/// What to do with a new settings file: the game server's lines, then the targets. `made` is
/// where the file was written, or None when it went to the command's output.
fn first_steps(out: &mut impl Write, made: Option<&Path>, token: &str) -> std::io::Result<()> {
    writeln!(out)?;
    match made {
        Some(path) => writeln!(
            out,
            "Made {} with a new token for your game server.",
            path.display()
        )?,
        None => writeln!(
            out,
            "That is your ciws.toml, with a new token for your game server."
        )?,
    }
    writeln!(out)?;
    writeln!(
        out,
        "1. In the game server's ServerSettings.ini, add (or change) these lines:"
    )?;
    writeln!(out)?;
    for line in config::game_ini(token).lines() {
        writeln!(out, "       {line}")?;
    }
    writeln!(out)?;
    for line in [
        "   Url is the address alone, with nothing after the port: the game adds",
        "   /api/ingest/events itself. 127.0.0.1 is right when CIWS runs on the same",
        "   machine as the game server. If the game server is elsewhere, or runs in",
        "   Docker itself, put the address of the machine CIWS runs on instead; across",
        "   the internet, an https:// address with a reverse proxy in front (see the",
        "   README). Then restart the game server: it reads these lines only at start.",
    ] {
        writeln!(out, "{line}")?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "2. Open ciws.toml and add a [[target]] for each place the kill feed should"
    )?;
    writeln!(
        out,
        "   go, then save it. CIWS picks the change up by itself."
    )?;
    writeln!(out)
}

/// Log times to the second, in UTC: what a person reading the console needs.
struct Seconds;

impl tracing_subscriber::fmt::time::FormatTime for Seconds {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(
            w,
            "{}",
            humantime::format_rfc3339_seconds(std::time::SystemTime::now())
        )
    }
}

fn run(path: PathBuf) -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        // Colour for a person at a terminal, plain text for Docker and journald.
        .with_ansi(std::io::stdout().is_terminal())
        .with_target(false)
        .with_timer(Seconds)
        .init();
    let config = match Config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            // Keep running and wait for the fix rather than exit: a window opened by a double
            // click would close before anyone could read why.
            error!("{e:#}");
            warn!("CIWS is waiting for that to be fixed and saved");
            match Config::parse("").and_then(Config::with_env) {
                Ok(c) => c,
                Err(e) => {
                    error!("{e:#}");
                    return pause_then(ExitCode::FAILURE);
                }
            }
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            error!("cannot start: {e}");
            return pause_then(ExitCode::FAILURE);
        }
    };
    runtime.block_on(async move {
        let relay = match Relay::start(config, Some(path)).await {
            Ok(r) => r,
            Err(e) => {
                error!("{e:#}");
                return pause_then(ExitCode::FAILURE);
            }
        };
        wait_for_signals(&relay).await;
        info!("stopping");
        relay.shutdown().await;
        ExitCode::SUCCESS
    })
}

/// On Windows a window opened by a double click closes when the program ends; keep it open long
/// enough to read why.
fn pause_then(code: ExitCode) -> ExitCode {
    #[cfg(windows)]
    if std::io::stdin().is_terminal() {
        eprintln!("Press Enter to close this window.");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    code
}

/// Until Ctrl-C or SIGTERM. SIGHUP re-reads the settings file at once (saving it does too).
#[cfg(unix)]
async fn wait_for_signals(relay: &Relay) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        error!("cannot listen for signals; stopping");
        return;
    };
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return,
            _ = term.recv() => return,
            _ = hup.recv() => {
                if let Err(e) = relay.reload_from_file().await {
                    error!("{e:#}");
                    warn!("the settings already running stay until that is fixed");
                }
            }
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_signals(_relay: &Relay) {
    let _ = tokio::signal::ctrl_c().await;
}

fn print_summary(c: &Config) {
    println!(
        "the game posts to   {} (Url=http://<this machine>:{})",
        c.listen,
        c.listen.port()
    );
    println!("status page         http://{}/status", c.admin_listen);
    if c.delivery.block_private {
        println!("targets             public addresses only");
    }
    if c.feeds.is_empty() {
        println!("\nno feeds: add a token and a [[target]]");
    }
    for f in &c.feeds {
        let label = if f.name == config::DEFAULT_FEED {
            "the kill feed".to_owned()
        } else {
            format!("feed {}", f.name)
        };
        if f.targets.is_empty() {
            println!("\n{label} goes nowhere yet: add a [[target]]");
            continue;
        }
        println!("\n{label} goes to:");
        for t in &f.targets {
            let path = if t.endpoint.path().ends_with(GAME_PATH) {
                "the game's path"
            } else {
                "its own path"
            };
            let auth = if t.authorization.is_some() {
                ", with a token"
            } else {
                ""
            };
            let age = t
                .max_age
                .map(|a| format!(", max_age {}", humantime::format_duration(a)))
                .unwrap_or_default();
            println!(
                "  {:<24} {}://{} ({path}{auth}{age})",
                t.name,
                t.endpoint.scheme(),
                t.host()
            );
        }
    }
}
