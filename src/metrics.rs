//! Counters for `/metrics` (Prometheus text format) and `/status`. Plain atomics, so nothing on
//! the path a batch takes waits on a lock to be counted.

use std::fmt::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::routes::{Lane, LaneShared, Routes};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostOutcome {
    Accepted,
    Unauthorized,
    RateLimited,
    TooLarge,
    NotABatch,
    Looped,
    /// The game's post on another path than its own: a `Url` with a path in it.
    WrongPath,
}

impl PostOutcome {
    pub const ALL: [PostOutcome; 7] = [
        PostOutcome::Accepted,
        PostOutcome::Unauthorized,
        PostOutcome::RateLimited,
        PostOutcome::TooLarge,
        PostOutcome::NotABatch,
        PostOutcome::Looped,
        PostOutcome::WrongPath,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PostOutcome::Accepted => "accepted",
            PostOutcome::Unauthorized => "unauthorized",
            PostOutcome::RateLimited => "rate_limited",
            PostOutcome::TooLarge => "too_large",
            PostOutcome::NotABatch => "not_a_batch",
            PostOutcome::Looped => "looped",
            PostOutcome::WrongPath => "wrong_path",
        }
    }
}

#[derive(Default)]
pub struct Metrics {
    posts: [AtomicU64; 7],
    pub events: AtomicU64,
    pub bytes: AtomicU64,
    /// time from a post's arrival to its answer, summed over accepted posts
    pub answer_us: AtomicU64,
}

impl Metrics {
    pub fn post(&self, outcome: PostOutcome) {
        self.posts[outcome as usize].fetch_add(1, Relaxed);
    }

    pub fn posts(&self, outcome: PostOutcome) -> u64 {
        self.posts[outcome as usize].load(Relaxed)
    }
}

pub fn render(m: &Metrics, routes: &Routes, now_ms: u64) -> String {
    let mut out = String::with_capacity(4096 + routes.lanes.len() * 1024);
    let w = &mut out;

    head(
        w,
        "ciws_posts_total",
        "counter",
        "Batches posted to the relay, by outcome.",
    );
    for o in PostOutcome::ALL {
        let _ = writeln!(
            w,
            "ciws_posts_total{{outcome=\"{}\"}} {}",
            o.label(),
            m.posts(o)
        );
    }
    single(
        w,
        "ciws_events_total",
        "counter",
        "Events in accepted batches.",
        m.events.load(Relaxed),
    );
    single(
        w,
        "ciws_received_bytes_total",
        "counter",
        "Bytes of accepted batches.",
        m.bytes.load(Relaxed),
    );
    let accepted = m.posts(PostOutcome::Accepted);
    head(
        w,
        "ciws_answer_seconds",
        "summary",
        "Time from a post's arrival to its answer, accepted posts.",
    );
    let _ = writeln!(
        w,
        "ciws_answer_seconds_sum {}",
        secs_us(m.answer_us.load(Relaxed))
    );
    let _ = writeln!(w, "ciws_answer_seconds_count {accepted}");

    head(
        w,
        "ciws_feed_posts_total",
        "counter",
        "Accepted batches per feed.",
    );
    for f in &routes.feeds {
        let _ = writeln!(
            w,
            "ciws_feed_posts_total{{feed=\"{}\"}} {}",
            f.name,
            f.stats.posts.load(Relaxed)
        );
    }
    head(
        w,
        "ciws_feed_events_total",
        "counter",
        "Events in accepted batches per feed.",
    );
    for f in &routes.feeds {
        let _ = writeln!(
            w,
            "ciws_feed_events_total{{feed=\"{}\"}} {}",
            f.name,
            f.stats.events.load(Relaxed)
        );
    }
    head(
        w,
        "ciws_feed_last_post_timestamp_seconds",
        "gauge",
        "When the feed last posted (0: not since start).",
    );
    for f in &routes.feeds {
        let at = f.stats.last_post_ms.load(Relaxed);
        let _ = writeln!(
            w,
            "ciws_feed_last_post_timestamp_seconds{{feed=\"{}\"}} {}",
            f.name,
            secs_ms(at)
        );
    }

    let mut lanes: Vec<_> = routes.lanes.values().collect();
    lanes.sort_by(|a, b| a.key.cmp(&b.key));
    head(
        w,
        "ciws_deliveries_total",
        "counter",
        "Batches a target is done with: delivered, refused by the target, expired (max_age), or \
         dropped from a full queue.",
    );
    for l in &lanes {
        let s = &l.shared.stats;
        for (outcome, n) in [
            ("delivered", &s.delivered),
            ("refused", &s.refused),
            ("expired", &s.expired),
            ("dropped", &s.dropped),
        ] {
            let _ = writeln!(
                w,
                "ciws_deliveries_total{{feed=\"{}\",target=\"{}\",outcome=\"{outcome}\"}} {}",
                l.feed,
                l.target.name,
                n.load(Relaxed)
            );
        }
    }
    lane_metric(
        w,
        &lanes,
        "ciws_delivery_failures_total",
        "counter",
        "Failed delivery attempts (each retry counted).",
        |l| l.stats.failures.load(Relaxed).to_string(),
    );
    lane_metric(
        w,
        &lanes,
        "ciws_delivery_backlog",
        "gauge",
        "Batches waiting for the target, counting the one being delivered.",
        |l| l.backlog().to_string(),
    );
    lane_metric(
        w,
        &lanes,
        "ciws_delivery_oldest_seconds",
        "gauge",
        "Age of the batch being delivered to the target (0 when none).",
        |l| match l.stats.head_ms.load(Relaxed) {
            0 => "0".into(),
            at => secs_ms(now_ms.saturating_sub(at)),
        },
    );
    lane_metric(
        w,
        &lanes,
        "ciws_delivery_lag_seconds_total",
        "counter",
        "Receipt to delivery, summed over delivered batches.",
        |l| secs_ms(l.stats.lag_ms_sum.load(Relaxed)),
    );
    out
}

fn lane_metric(
    w: &mut String,
    lanes: &[&Arc<Lane>],
    name: &str,
    kind: &str,
    help: &str,
    value: impl Fn(&LaneShared) -> String,
) {
    head(w, name, kind, help);
    for l in lanes {
        let _ = writeln!(
            w,
            "{name}{{feed=\"{}\",target=\"{}\"}} {}",
            l.feed,
            l.target.name,
            value(&l.shared)
        );
    }
}

fn head(w: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(w, "# HELP {name} {help}\n# TYPE {name} {kind}");
}

fn single(w: &mut String, name: &str, kind: &str, help: &str, value: impl std::fmt::Display) {
    head(w, name, kind, help);
    let _ = writeln!(w, "{name} {value}");
}

fn secs_us(us: u64) -> String {
    format!("{}.{:06}", us / 1_000_000, us % 1_000_000)
}

fn secs_ms(ms: u64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}
