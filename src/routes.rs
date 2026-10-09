//! The live routing table: which feed a token belongs to, and the lanes (one per feed × target)
//! its batches go out on. Built from the configuration and swapped whole on reload; a feed or
//! lane that survives a reload keeps its counters and its queue.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{Config, DEFAULT_FEED, POSTS_PER_MINUTE, TargetConfig, lane_key};
use crate::queue::Queue;
use crate::token;

pub struct Routes {
    pub feeds: Vec<Arc<Feed>>,
    by_token: HashMap<[u8; 32], Arc<Feed>>,
    pub lanes: HashMap<Arc<str>, Arc<Lane>>,
}

pub struct Feed {
    pub name: Arc<str>,
    /// "the kill feed", or "the kill feed of <name>" when there is more than one.
    pub label: String,
    pub lanes: Vec<Arc<Lane>>,
    pub stats: Arc<FeedStats>,
    pub limiter: RateLimiter,
}

pub struct Lane {
    /// `feed/target`: what a lane is known by across reloads.
    pub key: Arc<str>,
    /// What the log calls it: the target's name (and host, when the name is not the host), with
    /// its feed's name when there is more than one feed.
    pub label: String,
    pub feed: Arc<str>,
    pub target: TargetConfig,
    pub min_backoff: Duration,
    pub max_backoff: Duration,
    pub shared: Arc<LaneShared>,
}

impl Lane {
    /// Whether a running lane can carry on as it is under this lane's settings.
    pub fn same_settings(&self, other: &Lane) -> bool {
        self.target == other.target
            && self.min_backoff == other.min_backoff
            && self.max_backoff == other.max_backoff
    }
}

/// What a lane's task and the ingest route share: the queue, and the counters.
pub struct LaneShared {
    pub queue: Queue,
    pub stats: LaneStats,
}

impl LaneShared {
    /// Batches waiting, counting the one being delivered.
    pub fn backlog(&self) -> u64 {
        self.queue.len() as u64 + u64::from(self.stats.head_ms.load(Relaxed) != 0)
    }
}

#[derive(Default)]
pub struct FeedStats {
    pub posts: AtomicU64,
    pub events: AtomicU64,
    pub bytes: AtomicU64,
    pub last_post_ms: AtomicU64,
}

#[derive(Default)]
pub struct LaneStats {
    pub delivered: AtomicU64,
    pub refused: AtomicU64,
    pub expired: AtomicU64,
    /// pushed out of a full queue before the target could take them
    pub dropped: AtomicU64,
    /// failed attempts, each retry counted
    pub failures: AtomicU64,
    /// when the batch being delivered arrived; 0 while there is none
    pub head_ms: AtomicU64,
    pub last_ok_ms: AtomicU64,
    /// when the current run of failures began; 0 while the target is answering
    pub failing_since_ms: AtomicU64,
    /// the HTTP status of the last answer; 0 when the last attempt got none
    pub last_status: AtomicU32,
    /// [`crate::deliver::Failure::code`] of the last attempt that got no answer; 0 for none
    pub last_error: AtomicU8,
    /// receipt to delivery, summed over delivered batches
    pub lag_ms_sum: AtomicU64,
}

/// How the log speaks of a feed: one game server's needs no name.
pub fn feed_label(name: &str) -> String {
    if name == DEFAULT_FEED {
        "the kill feed".into()
    } else {
        format!("the kill feed of {name}")
    }
}

impl Routes {
    pub fn build(cfg: &Config, previous: Option<&Routes>) -> Routes {
        let mut feeds = Vec::with_capacity(cfg.feeds.len());
        let mut by_token = HashMap::with_capacity(cfg.feeds.len());
        let mut lanes = HashMap::new();
        for f in &cfg.feeds {
            let name: Arc<str> = Arc::from(f.name.as_str());
            let stats = previous
                .and_then(|p| p.feed(&name))
                .map(|old| old.stats.clone())
                .unwrap_or_default();
            let mut feed_lanes = Vec::with_capacity(f.targets.len());
            for t in &f.targets {
                let key: Arc<str> = Arc::from(lane_key(&f.name, &t.name));
                let shared = match previous.and_then(|p| p.lanes.get(&key)) {
                    Some(old) => {
                        old.shared.queue.set_capacity(cfg.delivery.queue);
                        old.shared.clone()
                    }
                    None => Arc::new(LaneShared {
                        queue: Queue::new(cfg.delivery.queue),
                        stats: LaneStats::default(),
                    }),
                };
                // "console.warcon.app", "warcon (console.warcon.app)", "server-2/warcon (…)".
                let label = if f.name == DEFAULT_FEED {
                    t.shown()
                } else {
                    format!("{}/{}", f.name, t.shown())
                };
                let lane = Arc::new(Lane {
                    key: key.clone(),
                    label,
                    feed: name.clone(),
                    target: t.clone(),
                    min_backoff: cfg.delivery.min_backoff,
                    max_backoff: cfg.delivery.max_backoff,
                    shared,
                });
                lanes.insert(key, lane.clone());
                feed_lanes.push(lane);
            }
            let feed = Arc::new(Feed {
                label: feed_label(&name),
                name,
                lanes: feed_lanes,
                stats,
                limiter: RateLimiter::per_minute(POSTS_PER_MINUTE),
            });
            by_token.insert(token::digest(f.token.as_bytes()), feed.clone());
            feeds.push(feed);
        }
        Routes {
            feeds,
            by_token,
            lanes,
        }
    }

    pub fn feed_for_token(&self, token: &[u8]) -> Option<&Arc<Feed>> {
        self.by_token.get(&token::digest(token))
    }

    pub fn feed(&self, name: &str) -> Option<&Arc<Feed>> {
        self.feeds.iter().find(|f| &*f.name == name)
    }

    pub fn lane_keys(&self) -> HashSet<Arc<str>> {
        self.lanes.keys().cloned().collect()
    }
}

/// A token bucket: `posts_per_minute` on average, with ten seconds' worth (at least ten) in hand.
/// The game posts about thirty times a minute at most, so this only ever stops a host in a loop.
pub struct RateLimiter {
    per_second: f64,
    burst: f64,
    state: Mutex<(f64, Instant)>,
}

impl RateLimiter {
    pub fn per_minute(n: u32) -> RateLimiter {
        let burst = (f64::from(n) / 6.0).max(10.0);
        RateLimiter {
            per_second: f64::from(n) / 60.0,
            burst,
            state: Mutex::new((burst, Instant::now())),
        }
    }

    pub fn allow(&self) -> bool {
        let mut s = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        s.0 = (s.0 + now.duration_since(s.1).as_secs_f64() * self.per_second).min(self.burst);
        s.1 = now;
        if s.0 >= 1.0 {
            s.0 -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
        [delivery]
        queue = 5
        [[feed]]
        name = "one"
        token = "0123456789abcdef-one"
        [[feed.target]]
        name = "a"
        url = "https://a.example"
        [[feed.target]]
        name = "b"
        url = "https://b.example"
        [[feed]]
        name = "two"
        token = "0123456789abcdef-two"
    "#;

    #[test]
    fn tokens_find_their_feed() {
        let routes = Routes::build(&Config::parse(CONFIG).unwrap(), None);
        assert_eq!(
            &*routes.feed_for_token(b"0123456789abcdef-one").unwrap().name,
            "one"
        );
        assert_eq!(
            &*routes.feed_for_token(b"0123456789abcdef-two").unwrap().name,
            "two"
        );
        assert!(routes.feed_for_token(b"0123456789abcdef-six").is_none());
        let mut keys: Vec<_> = routes
            .lane_keys()
            .into_iter()
            .map(|k| k.to_string())
            .collect();
        keys.sort();
        assert_eq!(keys, ["one/a", "one/b"]);
    }

    #[test]
    fn a_reload_keeps_what_survives_it() {
        let first = Routes::build(&Config::parse(CONFIG).unwrap(), None);
        first.feeds[0].stats.posts.store(7, Relaxed);
        first.lanes["one/a"]
            .shared
            .stats
            .delivered
            .store(5, Relaxed);
        let changed = CONFIG
            .replace("https://b.example", "https://c.example")
            .replace("queue = 5", "queue = 7");
        let second = Routes::build(&Config::parse(&changed).unwrap(), Some(&first));
        assert_eq!(second.feeds[0].stats.posts.load(Relaxed), 7);
        assert!(Arc::ptr_eq(
            &first.lanes["one/a"].shared,
            &second.lanes["one/a"].shared
        ));
        assert!(second.lanes["one/a"].same_settings(&first.lanes["one/a"]));
        assert!(!second.lanes["one/b"].same_settings(&first.lanes["one/b"]));
    }

    #[test]
    fn the_limiter_allows_a_burst_then_the_rate() {
        let l = RateLimiter::per_minute(60);
        let allowed = (0..100).filter(|_| l.allow()).count();
        assert_eq!(allowed, 10, "the burst is ten posts");
    }
}
