//! The endpoint the game posts to. To the game this is like any other feed endpoint: the path it
//! appends to `Url`, the bearer it was given, and a 200 as soon as the batch is on every target's
//! queue. Targets are never waited for here.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use tracing::{info, warn};

use crate::config::{GAME_PATH, MAX_BODY_BYTES, MAX_EVENTS, MAX_HOPS};
use crate::deliver::{self, RECEIVED_AT};
use crate::metrics::PostOutcome;
use crate::relay::App;
use crate::wire::{self, Batch, ShapeError};
use crate::{now_ms, token};

/// How far ahead of this relay's clock a received-at passed on by another relay may be.
const MAX_CLOCK_SKEW_MS: u64 = 5 * 60 * 1000;
const MAX_VIA_BYTES: usize = 1024;
const MAX_USER_AGENT_BYTES: usize = 512;
/// A feed quiet for this long is reported again when it next posts.
const QUIET_MS: u64 = 10 * 60 * 1000;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route(GAME_PATH, post(ingest))
        .route("/healthz", get(|| async { "ok" }))
        .fallback(elsewhere)
        .with_state(app)
}

/// Every other path is refused. A post that is plainly the game's but on the wrong path (its
/// `Url` was given a path, so the game added its own after it) is also named in the log: without
/// that, the feed just never arrives and nothing says why.
async fn elsewhere(State(app): State<Arc<App>>, request: Request) -> Response {
    let path = request.uri().path();
    if request.method() == Method::POST && path.ends_with(GAME_PATH) {
        app.metrics.post(PostOutcome::WrongPath);
        // The first and then every doubling, so a game posting every few seconds logs a few lines.
        if app.metrics.posts(PostOutcome::WrongPath).is_power_of_two() {
            let shown: String = path.chars().take(120).collect();
            warn!(
                "a batch was posted to {shown} and refused: CIWS takes it at {GAME_PATH} only. Set \
                 the game's [WDServerFeed] Url to the address alone, such as \
                 http://127.0.0.1:7780, with nothing after the port; the game adds {GAME_PATH} itself"
            );
        }
    }
    json(
        StatusCode::NOT_FOUND,
        r#"{"error":{"code":"not_found","message":"No such endpoint."}}"#.to_owned(),
    )
}

async fn ingest(State(app): State<Arc<App>>, request: Request) -> Response {
    let started = Instant::now();
    let headers = request.headers();
    let feed = {
        let routes = app.routes.load();
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| token::bearer(v.as_bytes()));
        token.and_then(|t| routes.feed_for_token(t)).cloned()
    };
    let Some(feed) = feed else {
        return refuse(
            &app,
            PostOutcome::Unauthorized,
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Unknown feed token.",
        );
    };

    let via = joined(headers, &header::VIA);
    if via.len() > MAX_VIA_BYTES || deliver::hops(&via) >= MAX_HOPS {
        return refuse(
            &app,
            PostOutcome::Looped,
            StatusCode::LOOP_DETECTED,
            "loop_detected",
            "This batch has been through too many relays.",
        );
    }
    if !feed.limiter.allow() {
        let mut r = refuse(
            &app,
            PostOutcome::RateLimited,
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many batches; slow down.",
        );
        r.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return r;
    }
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_BODY_BYTES as u64) {
        return refuse(
            &app,
            PostOutcome::TooLarge,
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "Batch too large.",
        );
    }
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() <= MAX_USER_AGENT_BYTES)
        .unwrap_or_default()
        .to_owned();
    let now = now_ms();
    // The game sends no time with its events. A batch passed on by another relay carries the time
    // the first relay took it, which is the one that means something.
    let received_at_ms = headers
        .get(RECEIVED_AT)
        .and_then(|v| v.to_str().ok())
        .and_then(deliver::parse_rfc3339_ms)
        .filter(|&at| at <= now + MAX_CLOCK_SKEW_MS)
        .unwrap_or(now);

    let Ok(body) = axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES).await else {
        return refuse(
            &app,
            PostOutcome::TooLarge,
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "Batch too large.",
        );
    };
    let events = match wire::check(&body, MAX_EVENTS) {
        Ok(n) => n as u64,
        Err(ShapeError::TooManyEvents) => {
            return refuse(
                &app,
                PostOutcome::NotABatch,
                StatusCode::BAD_REQUEST,
                "too_many_events",
                "Too many events in one batch.",
            );
        }
        Err(_) => {
            return refuse(
                &app,
                PostOutcome::NotABatch,
                StatusCode::BAD_REQUEST,
                "not_a_batch",
                "Expected a feed batch: { \"events\": [...] }.",
            );
        }
    };
    let bytes = body.len() as u64;

    let batch = Arc::new(Batch {
        received_at_ms,
        user_agent,
        via,
        // The body as read is a slice of the connection's read buffer, and a queued slice keeps
        // all of that buffer: 8 to 17 KB for a batch of half a kilobyte. A copy holds only itself.
        body: Bytes::copy_from_slice(&body),
    });
    for lane in &feed.lanes {
        let dropped = lane.shared.queue.push(batch.clone());
        if dropped > 0 {
            let total = lane.shared.stats.dropped.fetch_add(dropped, Relaxed) + dropped;
            // The first and then every doubling, so a long outage logs a few lines.
            if total.is_power_of_two() {
                warn!(
                    "{}: its queue is full, so the oldest batches are being dropped ({total} so far)",
                    lane.label
                );
            }
        }
    }

    let s = &feed.stats;
    s.posts.fetch_add(1, Relaxed);
    s.events.fetch_add(events, Relaxed);
    s.bytes.fetch_add(bytes, Relaxed);
    let previous = s.last_post_ms.swap(now, Relaxed);
    if previous == 0 || now.saturating_sub(previous) > QUIET_MS {
        // What someone setting this up waits to see: the game took its [WDServerFeed] lines.
        if feed.lanes.is_empty() {
            warn!(
                "{} is arriving, but it has nowhere to go yet: add a [[target]] to ciws.toml",
                feed.label
            );
        } else {
            info!("{} is arriving", feed.label);
        }
    }
    let m = &app.metrics;
    m.post(PostOutcome::Accepted);
    m.events.fetch_add(events, Relaxed);
    m.bytes.fetch_add(bytes, Relaxed);
    m.answer_us
        .fetch_add(started.elapsed().as_micros() as u64, Relaxed);
    json(StatusCode::OK, r#"{"ok":true}"#.to_owned())
}

/// Every value of a header that may repeat, joined as one list.
fn joined(headers: &HeaderMap, name: &HeaderName) -> String {
    let mut out = String::new();
    for v in headers.get_all(name).iter().filter_map(|v| v.to_str().ok()) {
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(v.trim());
    }
    out
}

/// The game's own error shape (`{ "error": { "code", "message" } }`, as its RCON API answers).
fn refuse(
    app: &App,
    outcome: PostOutcome,
    status: StatusCode,
    code: &str,
    message: &str,
) -> Response {
    app.metrics.post(outcome);
    json(
        status,
        serde_json::json!({ "error": { "code": code, "message": message } }).to_string(),
    )
}

pub(crate) fn json(status: StatusCode, body: String) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}
