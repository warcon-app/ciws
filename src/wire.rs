//! The one thing the relay checks about a body: that it is a feed batch, a JSON object with an
//! `events` array no longer than the limit. Nothing is decoded beyond that and the bytes are
//! passed on exactly as the game sent them.
//!
//! What the game posts (https://github.com/warcon-app/warcon/blob/main/docs/wardogs-api.md):
//! `{ "serverId": "<per-boot uuid>", "serverName": "...", "events": [ { "eventId", "type":
//! "killed", "eventTime", "matchId", "mapName", "killerName", "killerSteamId", "victimName",
//! "victimSteamId", "cause", "distance", "contextTags": [...] }, ... ] }`

use std::fmt;

use bytes::Bytes;
use serde::Deserialize;
use serde::de::{Deserializer, IgnoredAny, SeqAccess, Visitor};

/// An accepted post, held once however many targets it goes to.
pub struct Batch {
    /// When this relay took it (or the first relay, when it came through another), unix ms.
    pub received_at_ms: u64,
    /// The game's User-Agent, or empty.
    pub user_agent: String,
    /// The `Via` it arrived with, or empty.
    pub via: String,
    pub body: Bytes,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ShapeError {
    NotJson,
    NotABatch,
    TooManyEvents,
}

/// The number of events in a feed batch.
pub fn check(body: &[u8], max_events: usize) -> Result<usize, ShapeError> {
    let batch: Shape = serde_json::from_slice(body).map_err(|e| {
        if e.is_data() {
            ShapeError::NotABatch
        } else {
            ShapeError::NotJson
        }
    })?;
    if batch.events.0 > max_events {
        return Err(ShapeError::TooManyEvents);
    }
    Ok(batch.events.0)
}

/// All a body has to be.
#[derive(Deserialize)]
struct Shape {
    events: Count,
}

/// An array's length, read without keeping (or allocating for) its elements.
struct Count(usize);

impl<'de> Deserialize<'de> for Count {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Counter;
        impl<'de> Visitor<'de> for Counter {
            type Value = Count;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an array of events")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Count, A::Error> {
                let mut n = 0;
                while seq.next_element::<IgnoredAny>()?.is_some() {
                    n += 1;
                }
                Ok(Count(n))
            }
        }
        d.deserialize_seq(Counter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_events() {
        let body = br#"{"serverId":"e9cf","serverName":"[ABC] #1","events":[
            {"eventId":"a","type":"killed","eventTime":3317.77,"contextTags":["x"]},
            {"eventId":"b","type":"killed","eventTime":3318.1}]}"#;
        assert_eq!(check(body, 200), Ok(2));
        assert_eq!(check(br#"{"events":[]}"#, 200), Ok(0));
        assert_eq!(check(br#" {"events":[1,"two",null]} "#, 200), Ok(3));
    }

    #[test]
    fn refuses_what_is_not_a_batch() {
        assert_eq!(check(b"", 200), Err(ShapeError::NotJson));
        assert_eq!(check(b"{\"events\":[", 200), Err(ShapeError::NotJson));
        assert_eq!(
            check(b"{\"events\":[]} trailing", 200),
            Err(ShapeError::NotJson)
        );
        assert_eq!(check(b"[]", 200), Err(ShapeError::NotABatch));
        assert_eq!(
            check(b"{\"serverId\":\"x\"}", 200),
            Err(ShapeError::NotABatch)
        );
        assert_eq!(check(b"{\"events\":{}}", 200), Err(ShapeError::NotABatch));
        assert_eq!(
            check(b"{\"events\":[],\"events\":[]}", 200),
            Err(ShapeError::NotABatch)
        );
        assert_eq!(
            check(b"{\"events\":[1,2,3]}", 2),
            Err(ShapeError::TooManyEvents)
        );
    }
}
