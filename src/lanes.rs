//! Lanes: one task per feed × target, taking batches from that target's queue in the order they
//! arrived and delivering them one at a time, retrying with backoff while the target fails. A
//! slow or dead target holds up its own lane and nothing else: not the game, not other targets.

use std::collections::HashMap;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::deliver::{self, Outcome};
use crate::now_ms;
use crate::routes::{Lane, Routes};

/// The most requests in flight to one target host at once, across every lane that goes there.
/// Without a cap, a burst for many lanes to one plain-HTTP host opens a connection for each.
const PER_HOST: usize = 64;

pub struct LaneCtx {
    pub client: reqwest::Client,
    hosts: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl LaneCtx {
    pub fn new(client: reqwest::Client) -> LaneCtx {
        LaneCtx {
            client,
            hosts: Mutex::default(),
        }
    }

    /// The permits for this lane's host (scheme, host and port: one connection pool's worth).
    fn host_permits(&self, lane: &Lane) -> Arc<Semaphore> {
        let e = &lane.target.endpoint;
        let key = format!(
            "{}://{}:{}",
            e.scheme(),
            e.host_str().unwrap_or(""),
            e.port_or_known_default().unwrap_or(0)
        );
        let mut hosts = self.hosts.lock().unwrap_or_else(|p| p.into_inner());
        hosts
            .entry(key)
            .or_insert_with(|| Arc::new(Semaphore::new(PER_HOST)))
            .clone()
    }
}

pub struct Supervisor {
    ctx: Arc<LaneCtx>,
    shutdown: CancellationToken,
    running: Mutex<HashMap<Arc<str>, Running>>,
}

struct Running {
    lane: Arc<Lane>,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

impl Supervisor {
    pub fn new(ctx: LaneCtx) -> Supervisor {
        Supervisor {
            ctx: Arc::new(ctx),
            shutdown: CancellationToken::new(),
            running: Mutex::default(),
        }
    }

    /// Brings the running lanes in line with `routes`: a lane no longer there stops, one whose
    /// settings changed starts again (and so retries at once), a new one starts, and one whose
    /// task ended without being asked to is restarted.
    pub fn reconcile(&self, routes: &Routes) {
        if self.shutdown.is_cancelled() {
            return;
        }
        let mut running = self.running.lock().unwrap_or_else(|p| p.into_inner());
        running.retain(|key, r| {
            if r.handle.is_finished() {
                error!(lane = %key, "a lane stopped unexpectedly; restarting it");
                return false;
            }
            let keep = routes
                .lanes
                .get(key)
                .is_some_and(|l| l.same_settings(&r.lane));
            if !keep {
                r.cancel.cancel();
            }
            keep
        });
        for (key, lane) in &routes.lanes {
            if running.contains_key(key) {
                continue;
            }
            let cancel = self.shutdown.child_token();
            let handle = tokio::spawn(run(self.ctx.clone(), lane.clone(), cancel.clone()));
            running.insert(
                key.clone(),
                Running {
                    lane: lane.clone(),
                    cancel,
                    handle,
                },
            );
        }
    }

    /// Stops every lane; a batch a lane was delivering goes back on its queue.
    pub async fn stop_all(&self) {
        self.shutdown.cancel();
        let running: Vec<Running> = {
            let mut r = self.running.lock().unwrap_or_else(|p| p.into_inner());
            r.drain().map(|(_, v)| v).collect()
        };
        for r in &running {
            r.cancel.cancel();
        }
        for r in running {
            let _ = r.handle.await;
        }
    }
}

async fn run(ctx: Arc<LaneCtx>, lane: Arc<Lane>, cancel: CancellationToken) {
    let queue = &lane.shared.queue;
    let stats = &lane.shared.stats;
    let permits = ctx.host_permits(&lane);
    let mut failures: u32 = 0;
    let mut rng = seed();
    loop {
        let batch = loop {
            if let Some(b) = queue.pop() {
                break b;
            }
            stats.head_ms.store(0, Relaxed);
            tokio::select! {
                () = queue.wait() => {}
                () = cancel.cancelled() => return,
            }
        };
        stats.head_ms.store(batch.received_at_ms, Relaxed);

        // This batch until the target has it, refuses it, or it is too old to matter.
        loop {
            if let Some(max_age) = lane.target.max_age {
                if now_ms().saturating_sub(batch.received_at_ms) > max_age.as_millis() as u64 {
                    debug!(lane = %lane.key, "batch older than max_age; skipped");
                    stats.expired.fetch_add(1, Relaxed);
                    break;
                }
            }
            let outcome = tokio::select! {
                o = async {
                    let _permit = permits.acquire().await;
                    deliver::post(&ctx.client, &lane.target, &batch).await
                } => Some(o),
                () = cancel.cancelled() => None,
            };
            let Some(outcome) = outcome else {
                // Stopped mid-delivery (a reload changed this target, or shutdown): the batch
                // goes back for whoever takes the queue next.
                stats.head_ms.store(0, Relaxed);
                queue.put_back(batch);
                return;
            };
            let now = now_ms();
            match outcome {
                Outcome::Delivered(status) => {
                    stats.last_status.store(u32::from(status), Relaxed);
                    stats.last_error.store(0, Relaxed);
                    stats.last_ok_ms.store(now, Relaxed);
                    stats.failing_since_ms.store(0, Relaxed);
                    stats.delivered.fetch_add(1, Relaxed);
                    stats
                        .lag_ms_sum
                        .fetch_add(now.saturating_sub(batch.received_at_ms), Relaxed);
                    if failures > 0 {
                        info!("{}: delivering again", lane.label);
                    }
                    failures = 0;
                    break;
                }
                Outcome::Refused(status) => {
                    warn!(
                        "{} refused a batch (it answered {status}); that batch will not be sent \
                         there again",
                        lane.label
                    );
                    stats.last_status.store(u32::from(status), Relaxed);
                    stats.last_error.store(0, Relaxed);
                    stats.failing_since_ms.store(0, Relaxed);
                    stats.refused.fetch_add(1, Relaxed);
                    failures = 0;
                    break;
                }
                Outcome::Retry(failure, retry_after) => {
                    failures = failures.saturating_add(1);
                    stats.failures.fetch_add(1, Relaxed);
                    match failure {
                        deliver::Failure::Status(s) => {
                            stats.last_status.store(u32::from(s), Relaxed);
                            stats.last_error.store(0, Relaxed);
                        }
                        other => {
                            stats.last_status.store(0, Relaxed);
                            stats.last_error.store(other.code(), Relaxed);
                        }
                    }
                    if failures == 1 {
                        stats.failing_since_ms.store(now, Relaxed);
                    }
                    let wait = deliver::backoff(
                        failures,
                        retry_after,
                        lane.min_backoff,
                        lane.max_backoff,
                        jitter(&mut rng),
                    );
                    // The first failure and then every doubling, so a long outage logs a few lines.
                    if failures.is_power_of_two() {
                        warn!(
                            "{} {}; trying again in {}",
                            lane.label,
                            failure.phrase(),
                            short(wait)
                        );
                    }
                    if pause(wait, &cancel).await {
                        stats.head_ms.store(0, Relaxed);
                        queue.put_back(batch);
                        return;
                    }
                }
            }
        }
    }
}

/// "850ms", "4s", "5m".
fn short(d: Duration) -> String {
    match d.as_millis() {
        ms if ms < 1000 => format!("{ms}ms"),
        ms if ms < 60_000 => format!("{}s", (ms + 500) / 1000),
        ms => format!("{}m", (ms + 30_000) / 60_000),
    }
}

/// Sleeps, answering true when cancelled first.
async fn pause(wait: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        () = tokio::time::sleep(wait) => false,
        () = cancel.cancelled() => true,
    }
}

fn seed() -> u64 {
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    u64::from_le_bytes(b) | 1
}

/// 0.8 to 1.2, from a xorshift64 state.
fn jitter(state: &mut u64) -> f64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    0.8 + (*state >> 11) as f64 / (1u64 << 53) as f64 * 0.4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_durations() {
        let s = |ms| short(Duration::from_millis(ms));
        assert_eq!(
            [s(850), s(1000), s(4400), s(59_999), s(300_000)],
            ["850ms", "1s", "4s", "60s", "5m"]
        );
    }

    #[test]
    fn jitter_stays_in_range() {
        let mut s = seed();
        for _ in 0..10_000 {
            let j = jitter(&mut s);
            assert!((0.8..1.2).contains(&j), "{j}");
        }
    }
}
