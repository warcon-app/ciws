//! CIWS takes the kill feed a WARDOGS server posts to its one `[WDServerFeed] Url` and sends every
//! batch on to as many targets as are configured, each with its own token. In and out: the game
//! is answered as soon as the batch is on each target's queue, in memory, and a target that is
//! slow or briefly down catches up from its queue without holding up the game or the others.

pub mod admin;
pub mod config;
pub mod deliver;
pub mod ingest;
pub mod lanes;
pub mod metrics;
pub mod netguard;
pub mod queue;
pub mod relay;
pub mod routes;
pub mod token;
pub mod wire;

pub use config::Config;
pub use relay::Relay;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
