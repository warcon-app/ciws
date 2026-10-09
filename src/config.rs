//! `ciws.toml`. For one game server it is a `token` and a `[[target]]` for each place the kill feed
//! goes; more game servers are `[[feed]]` blocks, each with its own token and targets. Everything
//! else has a default that suits nearly everyone.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::header::{HeaderName, HeaderValue};
use serde::Deserialize;
use url::Url;

use crate::netguard;

/// What the game appends to `[WDServerFeed] Url`, and so where a target expects batches unless
/// its `path` says otherwise.
pub const GAME_PATH: &str = "/api/ingest/events";
pub const DEFAULT_PORT: u16 = 7780;
/// The feed a top-level `token` and `[[target]]`s make.
pub const DEFAULT_FEED: &str = "default";
/// The game's own listener takes no more than this; the relay matches it.
pub const MAX_BODY_BYTES: usize = 65_536;
/// The game has never sent more than ten events in one batch.
pub const MAX_EVENTS: usize = 200;
/// Per feed. The game posts about thirty times a minute at most, so this only stops a loop.
pub const POSTS_PER_MINUTE: u32 = 1200;
/// Relays a batch may already have passed through before it is refused as a loop.
pub const MAX_HOPS: usize = 4;

/// Headers the relay sets itself; a target's `headers` may not name them.
const RESERVED_HEADERS: &[&str] = &[
    "authorization",
    "connection",
    "content-length",
    "content-type",
    "host",
    "te",
    "transfer-encoding",
    "upgrade",
    "via",
    crate::deliver::RECEIVED_AT,
];

pub struct Config {
    pub listen: SocketAddr,
    pub admin_listen: SocketAddr,
    pub delivery: Delivery,
    pub feeds: Vec<FeedConfig>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Delivery {
    pub timeout: Duration,
    /// Batches each target may have waiting before the oldest are dropped.
    pub queue: usize,
    pub min_backoff: Duration,
    pub max_backoff: Duration,
    pub block_private: bool,
}

impl Default for Delivery {
    fn default() -> Self {
        Delivery {
            timeout: Duration::from_secs(10),
            queue: 1000,
            min_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(300),
            block_private: false,
        }
    }
}

#[derive(Clone)]
pub struct FeedConfig {
    pub name: String,
    pub token: String,
    pub targets: Vec<TargetConfig>,
}

impl fmt::Debug for FeedConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FeedConfig")
            .field("name", &self.name)
            .field("token", &"(hidden)")
            .field("targets", &self.targets)
            .finish()
    }
}

/// One destination of a feed. `authorization` and `headers` are marked sensitive, so `Debug`
/// prints them as `Sensitive`.
#[derive(Clone, Debug, PartialEq)]
pub struct TargetConfig {
    pub name: String,
    /// `url` with `path` appended: where every batch is POSTed.
    pub endpoint: Url,
    pub authorization: Option<HeaderValue>,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub max_age: Option<Duration>,
    pub timeout: Duration,
    /// Whether the file named it (otherwise the name is made from the host).
    pub named: bool,
}

impl TargetConfig {
    /// How logs speak of it: its name, and its host too when the name is not made from the host.
    pub fn shown(&self) -> String {
        if self.named {
            format!("{} ({})", self.name, self.host())
        } else {
            self.name.clone()
        }
    }

    /// The host (and port, when not the scheme's default) for logs and the status page. The full
    /// URL can carry a secret, as a Discord webhook's does, so it is never shown.
    pub fn host(&self) -> String {
        let host = self.endpoint.host_str().unwrap_or("?");
        match self.endpoint.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.to_owned(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    listen: Option<SocketAddr>,
    admin_listen: Option<SocketAddr>,
    token: Option<String>,
    #[serde(default, rename = "target")]
    targets: Vec<RawTarget>,
    #[serde(default, rename = "feed")]
    feeds: Vec<RawFeed>,
    #[serde(default)]
    delivery: RawDelivery,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFeed {
    name: String,
    token: String,
    #[serde(default, rename = "target")]
    targets: Vec<RawTarget>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTarget {
    name: Option<String>,
    url: String,
    token: Option<String>,
    path: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    max_age: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawDelivery {
    timeout: Option<String>,
    queue: Option<usize>,
    min_backoff: Option<String>,
    max_backoff: Option<String>,
    block_private: Option<bool>,
}

impl Config {
    /// Reads the file. `CIWS_LISTEN` and `CIWS_ADMIN_LISTEN` override `listen` and
    /// `admin_listen`, which is how the Docker image listens on every interface.
    pub fn load(path: &Path) -> Result<Config> {
        let bytes =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let text = decode(&bytes).with_context(|| {
            format!(
                "{} is not text CIWS can read: save it as UTF-8",
                path.display()
            )
        })?;
        Config::parse(&text)
            .with_context(|| format!("{} has a mistake", path.display()))?
            .with_env()
    }

    /// Applies `CIWS_LISTEN` and `CIWS_ADMIN_LISTEN`.
    pub fn with_env(mut self) -> Result<Config> {
        if let Ok(v) = std::env::var("CIWS_LISTEN") {
            self.listen = v
                .parse()
                .with_context(|| format!("CIWS_LISTEN={v:?} is not an address"))?;
        }
        if let Ok(v) = std::env::var("CIWS_ADMIN_LISTEN") {
            self.admin_listen = v
                .parse()
                .with_context(|| format!("CIWS_ADMIN_LISTEN={v:?} is not an address"))?;
        }
        Ok(self)
    }

    pub fn parse(text: &str) -> Result<Config> {
        let raw: Raw = toml::from_str(text)?;
        validate(raw)
    }

    /// The `feed/target` key of every lane.
    pub fn lane_keys(&self) -> impl Iterator<Item = String> + '_ {
        self.feeds
            .iter()
            .flat_map(|f| f.targets.iter().map(move |t| lane_key(&f.name, &t.name)))
    }
}

pub fn lane_key(feed: &str, target: &str) -> String {
    format!("{feed}/{target}")
}

/// The `[WDServerFeed]` lines for a game server on the same machine as the relay.
pub fn game_ini(token: &str) -> String {
    format!("[WDServerFeed]\nUrl=http://127.0.0.1:{DEFAULT_PORT}\nToken={token}")
}

/// The file the first run writes: a new token, and what to do with it.
pub fn starter(token: &str) -> String {
    let ini: String = game_ini(token)
        .lines()
        .map(|l| format!("#        {l}\n"))
        .collect();
    format!(
        r#"# CIWS sends your WARDOGS server's kill feed to every [[target]] listed below.
#
# 1. In your game server's ServerSettings.ini, add (or change) these lines:
#
{ini}#
#    Url is the address alone, with nothing after the port: the game adds
#    /api/ingest/events itself. 127.0.0.1 is right when CIWS runs on the same
#    machine as the game server. If the game server is elsewhere, or runs in
#    Docker itself, put the address of the machine CIWS runs on instead; across
#    the internet, an https:// address with a reverse proxy in front (see the
#    README). Then restart the game server: it reads these lines only at start.
#
# 2. Add a [[target]] below for each place the kill feed should go, and save this
#    file. CIWS picks the change up by itself.

# What the game sends as its Token. Keep it secret: whoever has it can send kills
# to your targets through CIWS.
token = "{token}"

# Warcon: the Token on your server's Config tab, under Kill feed.
# [[target]]
# url = "https://console.warcon.app"
# token = "wkf_..."

# Anything else that takes the game's kill feed: another panel, a stats site, a bot.
# [[target]]
# url = "https://stats.example.com"
# token = "..."
"#
    )
}

fn validate(raw: Raw) -> Result<Config> {
    let delivery = delivery(raw.delivery)?;

    let mut raw_feeds = Vec::with_capacity(raw.feeds.len() + 1);
    match raw.token {
        Some(token) => raw_feeds.push(RawFeed {
            name: DEFAULT_FEED.into(),
            token,
            targets: raw.targets,
        }),
        None if !raw.targets.is_empty() => bail!(
            "there are [[target]]s but no token: add a line token = \"...\" at the top, the same \
             as Token in the game's [WDServerFeed] (`ciws token` makes one)"
        ),
        None => {}
    }
    raw_feeds.extend(raw.feeds);

    let mut feeds: Vec<FeedConfig> = Vec::with_capacity(raw_feeds.len());
    for f in raw_feeds {
        if !valid_name(&f.name) {
            bail!(
                "feed name {:?}: use 1 to 64 letters, digits, '.', '_' or '-'",
                f.name
            );
        }
        if feeds.iter().any(|other| other.name == f.name) {
            bail!("two feeds are named {:?}", f.name);
        }
        let token = expand(&f.token).with_context(|| within(&f.name, "token".into()))?;
        if token.len() < 16 || token.len() > 512 || !visible_ascii(&token) {
            bail!(
                "{}",
                within(
                    &f.name,
                    "the token must be 16 to 512 characters, without spaces (`ciws token` makes one)"
                        .into()
                )
            );
        }
        if let Some(other) = feeds.iter().find(|other| other.token == token) {
            bail!(
                "{} and {} have the same token",
                feed_called(&other.name),
                feed_called(&f.name)
            );
        }
        let targets = targets(f.targets, &f.name, &delivery)?;
        feeds.push(FeedConfig {
            name: f.name,
            token,
            targets,
        });
    }

    Ok(Config {
        listen: raw
            .listen
            .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT))),
        admin_listen: raw
            .admin_listen
            .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT + 1))),
        delivery,
        feeds,
    })
}

fn delivery(raw: RawDelivery) -> Result<Delivery> {
    let d = Delivery::default();
    let delivery = Delivery {
        timeout: duration(raw.timeout, "delivery.timeout")?.unwrap_or(d.timeout),
        queue: raw.queue.unwrap_or(d.queue),
        min_backoff: duration(raw.min_backoff, "delivery.min_backoff")?.unwrap_or(d.min_backoff),
        max_backoff: duration(raw.max_backoff, "delivery.max_backoff")?.unwrap_or(d.max_backoff),
        block_private: raw.block_private.unwrap_or(d.block_private),
    };
    if delivery.timeout.is_zero() {
        bail!("delivery.timeout must be more than zero");
    }
    if !(1..=1_000_000).contains(&delivery.queue) {
        bail!("delivery.queue must be between 1 and 1000000");
    }
    if delivery.min_backoff.is_zero() || delivery.min_backoff > delivery.max_backoff {
        bail!("delivery.min_backoff must be more than zero and no more than max_backoff");
    }
    Ok(delivery)
}

fn targets(raw: Vec<RawTarget>, feed: &str, delivery: &Delivery) -> Result<Vec<TargetConfig>> {
    // Names written in the file first, so a name made from a host never takes one of them.
    let mut taken = HashSet::new();
    for t in &raw {
        if let Some(name) = &t.name {
            if !valid_name(name) {
                bail!(
                    "{}",
                    within(
                        feed,
                        format!(
                            "target name {name:?}: use 1 to 64 letters, digits, '.', '_' or '-'"
                        )
                    )
                );
            }
            if !taken.insert(name.clone()) {
                bail!(
                    "{}",
                    within(feed, format!("two targets are named {name:?}"))
                );
            }
        }
    }
    let mut out = Vec::with_capacity(raw.len());
    for (i, t) in raw.into_iter().enumerate() {
        // Never the URL: some carry a secret.
        let at = match &t.name {
            Some(name) => within(feed, format!("target {name:?}")),
            None => within(feed, format!("the {} [[target]]", ordinal(i + 1))),
        };
        let given = t.name.clone();
        let mut target = target(t, delivery).with_context(|| at)?;
        target.named = given.is_some();
        target.name = match given {
            Some(name) => name,
            None => {
                let base = name_from(&target.endpoint);
                let mut name = base.clone();
                let mut n = 2;
                while !taken.insert(name.clone()) {
                    name = format!("{base}-{n}");
                    n += 1;
                }
                name
            }
        };
        out.push(target);
    }
    Ok(out)
}

fn target(t: RawTarget, delivery: &Delivery) -> Result<TargetConfig> {
    let url = expand(&t.url).context("url")?;
    if !url.contains("://") {
        bail!("url must start with http:// or https://");
    }
    let mut endpoint = Url::parse(&url).map_err(|e| anyhow!("url is not a web address ({e})"))?;
    if !matches!(endpoint.scheme(), "http" | "https") {
        bail!("url must start with http:// or https://");
    }
    if endpoint.host().is_none() {
        bail!("url has no host");
    }
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        bail!("url must not carry a user name or password; use token or headers");
    }
    if endpoint.fragment().is_some() {
        bail!("url must not have a #fragment");
    }
    if delivery.block_private {
        netguard::check_literal_host(&endpoint).map_err(|why| anyhow!("url {why}"))?;
    }
    // The url means what the game's own Url means: the address alone, the game's path added after.
    // One that already ends with that path would be sent it twice; say so rather than guess.
    if t.path.is_none() && endpoint.path().trim_end_matches('/').ends_with(GAME_PATH) {
        bail!(
            "url ends with {GAME_PATH}, which CIWS adds itself: give the address alone, as for \
             the game's Url (or set path = \"\" to post to url exactly as written)"
        );
    }
    let path = t.path.as_deref().unwrap_or(GAME_PATH);
    if !path.is_empty() && !path.starts_with('/') {
        bail!("path must be empty or start with '/'");
    }
    // The same join the game makes with its own Url, minus a doubled slash.
    let joined = format!("{}{}", endpoint.path().trim_end_matches('/'), path);
    endpoint.set_path(if joined.is_empty() { "/" } else { &joined });

    let authorization = match t.token {
        None => None,
        Some(tok) => {
            let tok = expand(&tok).context("token")?;
            if tok.is_empty() || tok.len() > 4096 || !visible_ascii(&tok) {
                bail!("token must be 1 to 4096 characters, without spaces");
            }
            let mut v = HeaderValue::from_str(&format!("Bearer {tok}"))?;
            v.set_sensitive(true);
            Some(v)
        }
    };

    let mut headers = Vec::with_capacity(t.headers.len());
    for (k, v) in t.headers {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|_| anyhow!("headers: {k:?} is not a header name"))?;
        if RESERVED_HEADERS.contains(&name.as_str()) {
            bail!("headers: {name} is set by CIWS itself (use token for authorization)");
        }
        let v = expand(&v).with_context(|| format!("headers.{name}"))?;
        let mut value = HeaderValue::from_str(&v)
            .map_err(|_| anyhow!("headers.{name} is not a valid header value"))?;
        value.set_sensitive(true);
        headers.push((name, value));
    }

    let max_age = duration(t.max_age, "max_age")?;
    if max_age.is_some_and(|a| a < Duration::from_secs(1)) {
        bail!("max_age must be at least 1s");
    }

    Ok(TargetConfig {
        name: String::new(),
        named: false,
        endpoint,
        authorization,
        headers,
        max_age,
        timeout: delivery.timeout,
    })
}

/// A target's name when the file gives none: its host (and port), in the characters names use.
fn name_from(url: &Url) -> String {
    let host = url
        .host_str()
        .unwrap_or("target")
        .trim_matches(|c| c == '[' || c == ']');
    let name = match url.port() {
        Some(p) => format!("{host}-{p}"),
        None => host.to_owned(),
    };
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(56)
        .collect();
    if name.is_empty() {
        "target".into()
    } else {
        name
    }
}

/// An error's place in the file: as it is for the one-server shape, under its feed otherwise.
fn within(feed: &str, what: String) -> String {
    if feed == DEFAULT_FEED {
        what
    } else {
        format!("feed {feed:?}: {what}")
    }
}

fn feed_called(name: &str) -> String {
    if name == DEFAULT_FEED {
        "the token at the top".into()
    } else {
        format!("feed {name:?}")
    }
}

fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, 11) | (2, 12) | (3, 13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

fn valid_name(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn visible_ascii(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_graphic())
}

fn duration(v: Option<String>, field: &str) -> Result<Option<Duration>> {
    v.map(|s| {
        humantime::parse_duration(&s)
            .map_err(|_| anyhow!("{field}: {s:?} is not a duration such as \"10s\" or \"5m\""))
    })
    .transpose()
}

/// The file as text: UTF-8, with or without a byte-order mark (Notepad can add one), or UTF-16
/// with one (what Windows PowerShell's `>` writes).
fn decode(bytes: &[u8]) -> Option<String> {
    let utf16 = |rest: &[u8], unit: fn([u8; 2]) -> u16| {
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| unit([c[0], c[1]])).collect();
        (rest.len() % 2 == 0)
            .then(|| String::from_utf16(&units).ok())
            .flatten()
    };
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8(rest.to_vec()).ok(),
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        _ => String::from_utf8(bytes.to_vec()).ok(),
    }
}

/// Replaces each `${NAME}` with that environment variable, so secrets can stay out of the file.
fn expand(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let end = after.find('}').ok_or_else(|| anyhow!("unclosed \"${{\""))?;
        let name = &after[..end];
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            bail!("${{{name}}} is not an environment variable name");
        }
        let value =
            std::env::var(name).map_err(|_| anyhow!("environment variable {name} is not set"))?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "cws_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn simple(targets: &str) -> String {
        format!("token = \"{TOKEN}\"\n{targets}")
    }

    fn err(text: &str) -> String {
        format!(
            "{:#}",
            Config::parse(text).err().expect("expected an error")
        )
    }

    #[test]
    fn defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.listen, "0.0.0.0:7780".parse().unwrap());
        assert_eq!(c.admin_listen, "127.0.0.1:7781".parse().unwrap());
        assert_eq!(c.delivery, Delivery::default());
        assert!(c.feeds.is_empty());
    }

    #[test]
    fn the_starter_file_is_valid_and_carries_its_token() {
        let c = Config::parse(&starter(TOKEN)).unwrap();
        assert_eq!(c.feeds.len(), 1);
        assert_eq!(c.feeds[0].name, DEFAULT_FEED);
        assert_eq!(c.feeds[0].token, TOKEN);
        assert!(c.feeds[0].targets.is_empty());
        assert!(starter(TOKEN).contains(&format!("#        Token={TOKEN}\n")));
    }

    #[test]
    fn one_server_needs_only_a_token_and_targets() {
        let c = Config::parse(&simple(
            r#"
            [[target]]
            url = "https://console.warcon.app"
            token = "wkf_one"
            [[target]]
            url = "https://console.warcon.app"
            token = "wkf_two"
            [[target]]
            url = "http://10.0.0.5:3000"
            "#,
        ))
        .unwrap();
        let t = &c.feeds[0].targets;
        let names: Vec<_> = t.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "console.warcon.app",
                "console.warcon.app-2",
                "10.0.0.5-3000"
            ]
        );
        assert_eq!(
            t[0].authorization.as_ref().unwrap().as_bytes(),
            b"Bearer wkf_one"
        );
        assert_eq!(
            t[1].authorization.as_ref().unwrap().as_bytes(),
            b"Bearer wkf_two"
        );
        assert_eq!(
            t[0].endpoint, t[1].endpoint,
            "the same server twice, with two tokens"
        );
    }

    #[test]
    fn names_given_in_the_file_win() {
        let c = Config::parse(&simple(
            r#"
            [[target]]
            url = "https://a.example"
            [[target]]
            name = "a.example"
            url = "https://b.example"
            "#,
        ))
        .unwrap();
        let names: Vec<_> = c.feeds[0].targets.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["a.example-2", "a.example"]);
    }

    #[test]
    fn more_servers_are_feed_blocks() {
        let c = Config::parse(&format!(
            "{}[[feed]]\nname = \"server-2\"\ntoken = \"0123456789abcdef-two\"\n\
             [[feed.target]]\nurl = \"https://a.example\"\n",
            simple("[[target]]\nurl = \"https://a.example\"\n")
        ))
        .unwrap();
        let feeds: Vec<_> = c
            .feeds
            .iter()
            .map(|f| (f.name.as_str(), f.targets.len()))
            .collect();
        assert_eq!(feeds, [(DEFAULT_FEED, 1), ("server-2", 1)]);
        let keys: Vec<_> = c.lane_keys().collect();
        assert_eq!(keys, ["default/a.example", "server-2/a.example"]);
    }

    #[test]
    fn target_endpoint_follows_the_game() {
        let c = Config::parse(&simple(
            r#"
            [[target]]
            url = "https://console.warcon.app"
            [[target]]
            url = "https://example.com/base/"
            [[target]]
            url = "https://example.com/hook?key=1"
            path = ""
            [[target]]
            name = "custom"
            url = "http://10.0.0.5:3000/x"
            path = "/kills"
            max_age = "15m"
            headers = { "X-Api-Key" = "k" }
            "#,
        ))
        .unwrap();
        let t = &c.feeds[0].targets;
        assert_eq!(
            t[0].endpoint.as_str(),
            "https://console.warcon.app/api/ingest/events"
        );
        let whole = "[[target]]\nurl = \"https://console.warcon.app/api/ingest/events\"\n";
        assert!(err(&simple(whole)).contains("which CIWS adds itself"));
        let exact = Config::parse(&simple(
            "[[target]]\nurl = \"https://console.warcon.app/api/ingest/events\"\npath = \"\"\n",
        ))
        .unwrap();
        assert_eq!(
            exact.feeds[0].targets[0].endpoint.as_str(),
            "https://console.warcon.app/api/ingest/events",
            "path = \"\" posts to the url as written"
        );
        assert_eq!(
            t[1].endpoint.as_str(),
            "https://example.com/base/api/ingest/events"
        );
        assert_eq!(t[2].endpoint.as_str(), "https://example.com/hook?key=1");
        assert_eq!(t[3].endpoint.as_str(), "http://10.0.0.5:3000/x/kills");
        assert_eq!(t[3].max_age, Some(Duration::from_secs(900)));
        assert_eq!(t[3].timeout, Duration::from_secs(10));
        assert_eq!(t[3].headers[0].0.as_str(), "x-api-key");
        assert_eq!(t[3].host(), "10.0.0.5:3000");
        assert!(t[2].authorization.is_none());
    }

    #[test]
    fn secrets_never_reach_debug_output() {
        let c = Config::parse(&simple(
            "[[target]]\nurl = \"https://a.example\"\ntoken = \"wkf_secret\"\n\
             headers = { \"X-Key\" = \"hush\" }\n",
        ))
        .unwrap();
        let shown = format!("{:?}", c.feeds);
        assert!(!shown.contains(TOKEN));
        assert!(!shown.contains("wkf_secret"));
        assert!(!shown.contains("hush"));
    }

    #[test]
    fn environment_expansion() {
        // SAFETY: the variable names are unique to this test.
        unsafe { std::env::set_var("CIWS_TEST_TARGET_TOKEN", "wkf_from_env") };
        let c = Config::parse(&simple(
            "[[target]]\nurl = \"https://a.example\"\ntoken = \"${CIWS_TEST_TARGET_TOKEN}\"\n",
        ))
        .unwrap();
        let auth = c.feeds[0].targets[0].authorization.as_ref().unwrap();
        assert_eq!(auth.as_bytes(), b"Bearer wkf_from_env");
        assert!(
            err(&simple(
                "[[target]]\nurl = \"https://a.example\"\ntoken = \"${CIWS_TEST_UNSET_VARIABLE}\"\n"
            ))
            .contains("CIWS_TEST_UNSET_VARIABLE is not set")
        );
    }

    #[test]
    fn mistakes_are_explained() {
        assert!(err("[[target]]\nurl = \"https://a.example\"").contains("no token"));
        assert!(err("token = \"short\"").contains("16 to 512"));
        assert!(
            err(&simple("[[target]]\nurl = \"ftp://x\""))
                .contains("the 1st [[target]]: url must start with http")
        );
        let no_scheme = "[[target]]\nurl = \"https://x\"\n[[target]]\nurl = \"console.warcon.app\"";
        assert!(
            err(&simple(no_scheme))
                .contains("the 2nd [[target]]: url must start with http:// or https://")
        );
        assert!(
            err(&simple("[[target]]\nurl = \"https://\"")).contains("url is not a web address")
        );
        assert!(err(&simple("[[target]]\nurl = \"https://u:p@x\"")).contains("user name"));
        assert!(err(&simple("[[target]]\nurl = \"https://x\"\npath = \"api\"")).contains("path"));
        assert!(
            err(&simple(
                "[[target]]\nurl = \"https://x\"\nheaders = { Authorization = \"x\" }"
            ))
            .contains("set by CIWS")
        );
        assert!(err(&simple("[[target]]\nname = \"a\"\nurl = \"https://x\"\n[[target]]\nname = \"a\"\nurl = \"https://y\"")).contains("two targets"));
        assert!(
            err(&simple(
                "[[target]]\nurl = \"https://x\"\nmax_age = \"soon\""
            ))
            .contains("duration")
        );
        assert!(
            err(&format!(
                "{}[[feed]]\nname = \"b\"\ntoken = \"{TOKEN}\"",
                simple("")
            ))
            .contains("same token")
        );
        assert!(
            err("[[feed]]\nname = \"a b\"\ntoken = \"0123456789abcdef\"").contains("feed name")
        );
        assert!(err("bogus = 1").contains("unknown field"));
        // A URL can hold a secret, so no message repeats one.
        assert!(
            !err(&simple(
                "[[target]]\nurl = \"https://discord.com/api/webhooks/1/s3cret#x\""
            ))
            .contains("s3cret")
        );
    }

    #[test]
    fn a_target_may_reuse_the_feed_token() {
        // The easy move for a Warcon user: point the game's Url at CIWS and keep its Token, so
        // the same wkf_ token is the feed's and the Warcon target's. A target that really is
        // this relay is stopped by the Via hop limit instead.
        let text = simple(&format!(
            "[[target]]\nurl = \"https://console.warcon.app\"\ntoken = \"{TOKEN}\"\n"
        ));
        assert!(Config::parse(&text).is_ok());
    }

    #[test]
    fn block_private_refuses_internal_literals() {
        let with = |url: &str| {
            format!(
                "{}[delivery]\nblock_private = true\n",
                simple(&format!("[[target]]\nurl = \"{url}\"\n"))
            )
        };
        assert!(err(&with("http://127.0.0.1:3000")).contains("private or internal"));
        assert!(err(&with("http://localhost:3000")).contains("localhost"));
        assert!(Config::parse(&with("https://console.warcon.app")).is_ok());
    }

    #[test]
    fn files_saved_on_windows_are_read() {
        let text = "token = \"0123456789abcdef\"\n";
        let le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        let be: Vec<u8> = [0xFE, 0xFF]
            .into_iter()
            .chain(text.encode_utf16().flat_map(u16::to_be_bytes))
            .collect();
        let bom: Vec<u8> = [0xEF, 0xBB, 0xBF].into_iter().chain(text.bytes()).collect();
        for bytes in [text.as_bytes().to_vec(), bom, le, be] {
            assert_eq!(decode(&bytes).as_deref(), Some(text));
        }
        assert_eq!(decode(&[0xFF, 0xFE, 0x41]), None, "half a UTF-16 unit");
        assert_eq!(decode(&[0xC3, 0x28]), None, "not UTF-8");
    }

    #[test]
    fn ordinals() {
        let got: Vec<_> = [1, 2, 3, 4, 11, 12, 13, 21, 22, 101, 111]
            .map(ordinal)
            .to_vec();
        assert_eq!(
            got,
            [
                "1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "101st",
                "111th"
            ]
        );
    }
}
