//! The admin listener (127.0.0.1:7781 unless configured otherwise): `/status` for a person,
//! `/metrics` for Prometheus, and `/healthz`. It names feeds and target hosts, so it is kept off
//! the public listener.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Map, Value, json};

use crate::deliver::Failure;
use crate::ingest::json as json_response;
use crate::metrics::{PostOutcome, render};
use crate::relay::Control;
use crate::{VERSION, now_ms};

pub fn router(control: Arc<Control>) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .route("/status", get(status))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(control)
}

async fn metrics(State(c): State<Arc<Control>>) -> Response {
    let routes = c.app.routes.load_full();
    let body = render(&c.app.metrics, &routes, now_ms());
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

async fn status(State(c): State<Arc<Control>>) -> Response {
    let routes = c.app.routes.load_full();
    let now = now_ms();
    let ago = |ms: u64| (ms != 0).then(|| now.saturating_sub(ms) as f64 / 1000.0);
    let feeds: Vec<Value> = routes
        .feeds
        .iter()
        .map(|f| {
            let targets: Vec<Value> = f
                .lanes
                .iter()
                .map(|l| {
                    let s = &l.shared.stats;
                    let failing_since = s.failing_since_ms.load(Relaxed);
                    let backlog = l.shared.backlog();
                    let state = if failing_since != 0 {
                        "failing"
                    } else if backlog > 0 {
                        "behind"
                    } else {
                        "ok"
                    };
                    let last_status = s.last_status.load(Relaxed);
                    json!({
                        "name": l.target.name,
                        "host": l.target.host(),
                        "state": state,
                        "backlog": backlog,
                        "oldest_s": ago(s.head_ms.load(Relaxed)),
                        "delivered": s.delivered.load(Relaxed),
                        "refused": s.refused.load(Relaxed),
                        "expired": s.expired.load(Relaxed),
                        "dropped": s.dropped.load(Relaxed),
                        "failed_attempts": s.failures.load(Relaxed),
                        "last_ok_s_ago": ago(s.last_ok_ms.load(Relaxed)),
                        "failing_for_s": ago(failing_since),
                        "last_status": (last_status != 0).then_some(last_status),
                        "last_error": Failure::from_code(s.last_error.load(Relaxed)).map(Failure::phrase),
                    })
                })
                .collect();
            json!({
                "name": &*f.name,
                "posts": f.stats.posts.load(Relaxed),
                "events": f.stats.events.load(Relaxed),
                "last_post_s_ago": ago(f.stats.last_post_ms.load(Relaxed)),
                "targets": targets,
            })
        })
        .collect();
    let posts: Map<String, Value> = PostOutcome::ALL
        .iter()
        .map(|o| (o.label().to_owned(), json!(c.app.metrics.posts(*o))))
        .collect();
    let body = json!({
        "version": VERSION,
        "uptime_s": now.saturating_sub(c.app.started_ms) / 1000,
        "posts": posts,
        "feeds": feeds,
    });
    json_response(StatusCode::OK, body.to_string())
}
