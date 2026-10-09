//! One POST of one batch to one target, and what the answer means for the lane.
//!
//! The request is the game's own, as near as a relay can make it: the body byte for byte, the
//! game's User-Agent, `Content-Type: application/json`, and the target's own bearer. A receiver
//! built for the game (Warcon is one) needs no change. Two headers are added: `Via`, so relays
//! chained into a loop are caught, and `X-Ciws-Received-At`, the wall-clock time the relay took
//! the batch, which the game's events do not carry and which matters when a delivery is late.

use std::borrow::Cow;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER, USER_AGENT, VIA};

use crate::config::{Delivery, TargetConfig};
use crate::netguard;
use crate::wire::Batch;

pub const RECEIVED_AT: &str = "x-ciws-received-at";
/// What this relay adds to `Via` (RFC 9110 §7.6.3). Hops are counted by it.
pub const VIA_SELF: &str = "1.1 ciws";
pub const FALLBACK_USER_AGENT: &str = concat!("ciws/", env!("CARGO_PKG_VERSION"));
/// How much of an answer is read (and thrown away) so the connection can be reused.
const MAX_ANSWER_READ: usize = 64 * 1024;
/// The longest `Retry-After` honoured; beyond it the lane's own backoff applies.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// An answer that means "not now".
    Status(u16),
    Timeout,
    Connect,
    Other,
}

impl Failure {
    /// What the status page shows. Fixed phrases: an error's own text can carry the target URL.
    pub fn phrase(self) -> Cow<'static, str> {
        match self {
            Failure::Status(s) => Cow::Owned(format!("answered {s}")),
            Failure::Timeout => Cow::Borrowed("timed out"),
            Failure::Connect => Cow::Borrowed("could not connect"),
            Failure::Other => Cow::Borrowed("request failed"),
        }
    }

    pub fn code(self) -> u8 {
        match self {
            Failure::Status(_) => 0,
            Failure::Timeout => 1,
            Failure::Connect => 2,
            Failure::Other => 3,
        }
    }

    pub fn from_code(code: u8) -> Option<Failure> {
        match code {
            1 => Some(Failure::Timeout),
            2 => Some(Failure::Connect),
            3 => Some(Failure::Other),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Delivered(u16),
    /// The target turned this batch down (400, 413, 422): sending it again cannot help.
    Refused(u16),
    /// Try again later, after the `Retry-After` the target asked for if any.
    Retry(Failure, Option<Duration>),
}

/// Makes ring the process's TLS crypto, once; any HTTP client needs it before it is built.
pub fn use_ring() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn client(d: &Delivery) -> Result<Client> {
    use_ring();
    let mut builder = Client::builder()
        .connect_timeout(d.timeout.min(Duration::from_secs(5)))
        // The game follows a redirect as a GET, which no feed endpoint accepts; a relay that
        // followed one would hide a misconfigured URL. A 3xx is retried and shows as a failure.
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .tcp_nodelay(true)
        .pool_idle_timeout(Duration::from_secs(90));
    if d.block_private {
        builder = builder.dns_resolver(netguard::PublicOnly);
    }
    Ok(builder.build()?)
}

pub async fn post(client: &Client, t: &TargetConfig, rec: &Batch) -> Outcome {
    let user_agent = if rec.user_agent.is_empty() {
        FALLBACK_USER_AGENT
    } else {
        &rec.user_agent
    };
    let via = if rec.via.is_empty() {
        Cow::Borrowed(VIA_SELF)
    } else {
        Cow::Owned(format!("{}, {VIA_SELF}", rec.via))
    };
    let mut req = client
        .post(t.endpoint.clone())
        .timeout(t.timeout)
        .header(CONTENT_TYPE, "application/json")
        .header(USER_AGENT, user_agent)
        .header(VIA, via.as_ref())
        .header(RECEIVED_AT, rfc3339_ms(rec.received_at_ms));
    if let Some(auth) = &t.authorization {
        req = req.header(AUTHORIZATION, auth.clone());
    }
    for (name, value) in &t.headers {
        req = req.header(name.clone(), value.clone());
    }
    let mut resp = match req.body(rec.body.clone()).send().await {
        Ok(r) => r,
        Err(e) if e.is_builder() => {
            // Every part of the request was checked on the way in, so this is a defect, and
            // sending the same request again would fail the same way.
            tracing::error!(error = %e.without_url(), "could not build a delivery request");
            return Outcome::Refused(0);
        }
        Err(e) => {
            let failure = if e.is_timeout() {
                Failure::Timeout
            } else if e.is_connect() {
                Failure::Connect
            } else {
                Failure::Other
            };
            tracing::debug!(error = %e.without_url(), "delivery request failed");
            return Outcome::Retry(failure, None);
        }
    };
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(retry_after_secs);
    // Read a little of the answer so the connection goes back to the pool. Whatever it says
    // is never stored or shown: the status is the whole of what a target tells the relay.
    let mut left = MAX_ANSWER_READ;
    while let Ok(Some(chunk)) = resp.chunk().await {
        if chunk.len() >= left {
            break;
        }
        left -= chunk.len();
    }
    classify(status, retry_after)
}

pub fn classify(status: u16, retry_after: Option<Duration>) -> Outcome {
    match status {
        200..=299 => Outcome::Delivered(status),
        400 | 413 | 422 => Outcome::Refused(status),
        // Everything else might come right: a 5xx or 429 by itself, a 401, 404 or 3xx once the
        // target or this relay's configuration is fixed. Keep the batch and keep trying.
        _ => Outcome::Retry(Failure::Status(status), retry_after),
    }
}

fn retry_after_secs(v: &str) -> Option<Duration> {
    v.trim()
        .parse::<u64>()
        .ok()
        .map(|s| Duration::from_secs(s).min(MAX_RETRY_AFTER))
}

/// The wait before the next attempt: doubling from `min` up to `max`, spread by `jitter`
/// (0.8 to 1.2) so lanes that failed together do not retry together; a target's `Retry-After`,
/// within the same bounds, instead.
pub fn backoff(
    failures: u32,
    retry_after: Option<Duration>,
    min: Duration,
    max: Duration,
    jitter: f64,
) -> Duration {
    if let Some(wait) = retry_after {
        return wait.clamp(min, max);
    }
    let doubled = min.saturating_mul(1 << failures.saturating_sub(1).min(20));
    doubled.min(max).mul_f64(jitter).clamp(min, max)
}

/// How many relays a request already went through, by their `Via` entries.
pub fn hops(via: &str) -> usize {
    via.split(',')
        .filter(|entry| entry.split_whitespace().nth(1) == Some("ciws"))
        .count()
}

pub fn rfc3339_ms(ms: u64) -> String {
    humantime::format_rfc3339_millis(UNIX_EPOCH + Duration::from_millis(ms)).to_string()
}

pub fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    let t: SystemTime = humantime::parse_rfc3339(s.trim()).ok()?;
    Some(t.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_classified() {
        assert_eq!(classify(200, None), Outcome::Delivered(200));
        assert_eq!(classify(204, None), Outcome::Delivered(204));
        assert_eq!(classify(400, None), Outcome::Refused(400));
        assert_eq!(classify(413, None), Outcome::Refused(413));
        assert_eq!(classify(422, None), Outcome::Refused(422));
        for s in [301, 302, 401, 403, 404, 408, 429, 500, 502, 503, 520] {
            assert!(
                matches!(classify(s, None), Outcome::Retry(Failure::Status(x), None) if x == s)
            );
        }
        let wait = Some(Duration::from_secs(7));
        assert_eq!(
            classify(429, wait),
            Outcome::Retry(Failure::Status(429), wait)
        );
    }

    #[test]
    fn retry_after_is_seconds_and_capped() {
        assert_eq!(retry_after_secs("30"), Some(Duration::from_secs(30)));
        assert_eq!(retry_after_secs(" 5 "), Some(Duration::from_secs(5)));
        assert_eq!(retry_after_secs("999999"), Some(MAX_RETRY_AFTER));
        assert_eq!(retry_after_secs("Wed, 21 Oct 2015 07:28:00 GMT"), None);
    }

    #[test]
    fn backoff_doubles_within_bounds() {
        let (min, max) = (Duration::from_secs(1), Duration::from_secs(300));
        let at = |n| backoff(n, None, min, max, 1.0).as_secs();
        assert_eq!(
            [at(1), at(2), at(3), at(4), at(9), at(10), at(50)],
            [1, 2, 4, 8, 256, 300, 300]
        );
        assert_eq!(backoff(1, None, min, max, 0.8), min, "never under min");
        assert_eq!(backoff(20, None, min, max, 1.2), max, "never over max");
        assert_eq!(
            backoff(1, Some(Duration::from_secs(30)), min, max, 1.0),
            Duration::from_secs(30)
        );
        assert_eq!(
            backoff(1, Some(Duration::from_secs(3600)), min, max, 1.0),
            max
        );
        assert_eq!(backoff(u32::MAX, None, min, max, 1.0), max);
    }

    #[test]
    fn via_hops() {
        assert_eq!(hops(""), 0);
        assert_eq!(hops("1.1 ciws"), 1);
        assert_eq!(hops("1.1 ciws, 1.1 cloudflare, 1.1 ciws"), 2);
        assert_eq!(hops("1.1 varnish"), 0);
    }

    #[test]
    fn timestamps_round_trip() {
        let ms = 1_791_000_123_456;
        let s = rfc3339_ms(ms);
        assert!(s.ends_with(".456Z"), "{s}");
        assert_eq!(parse_rfc3339_ms(&s), Some(ms));
        assert_eq!(parse_rfc3339_ms("yesterday"), None);
    }
}
