//! The relay end to end: real listeners, targets that answer as scripted.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::routing::post;
use ciws::{Config, Relay};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const GAME_UA: &str = "Wardogs/++Wardogs+Live-CL-501228 (http-eventloop) Linux/debian12";
const FEED_TOKEN: &str = "cws_test_feed_token_0123456789abcdefghijklmnopqrs";

fn batch(n: usize) -> String {
    format!(
        r#"{{"serverId":"e9cf2544-0000-4000-8000-000000000000","serverName":"[ABC] Example Server #1","events":[{{"eventId":"{n:08}-0000-4000-8000-000000000000","type":"killed","eventTime":{n}.5,"matchId":"m","mapName":"Kavkazi","killerName":"Night Owl","killerId":"-ab0d","killerSteamId":"76561198100000101","victimName":"Ghostpepper","victimId":"-cd12","victimSteamId":"76561198100000102","cause":"Id.Item.AK74M","distance":4012.5,"contextTags":["Meta.Progression.Context.Player.KillContext.Headshot","Meta.PlayerKillFlag.Player.Local.Kill","Meta.PlayerKillFlag.Player.Local.Death"]}}]}}"#
    )
}

// ---- a scripted target ----

#[derive(Clone, Debug)]
struct Got {
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

impl Got {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

#[derive(Default)]
struct TargetState {
    got: Mutex<Vec<Got>>,
    /// answers to give, in order, as (status, delay); 200 at once after they run out
    script: Mutex<VecDeque<(u16, u64)>>,
}

struct Target {
    addr: SocketAddr,
    state: Arc<TargetState>,
    server: JoinHandle<()>,
}

impl Target {
    async fn start() -> Target {
        Target::on(TcpListener::bind("127.0.0.1:0").await.unwrap()).await
    }

    async fn on(listener: TcpListener) -> Target {
        let state = Arc::new(TargetState::default());
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .fallback(post(answer))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Target {
            addr,
            state,
            server,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn script(&self, answers: &[(u16, u64)]) {
        self.state
            .script
            .lock()
            .unwrap()
            .extend(answers.iter().copied());
    }

    fn got(&self) -> Vec<Got> {
        self.state.got.lock().unwrap().clone()
    }

    fn bodies(&self) -> Vec<String> {
        self.got()
            .iter()
            .map(|g| String::from_utf8(g.body.to_vec()).unwrap())
            .collect()
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn answer(
    State(s): State<Arc<TargetState>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let (status, delay) = s.script.lock().unwrap().pop_front().unwrap_or((200, 0));
    s.got.lock().unwrap().push(Got {
        path: uri.to_string(),
        headers,
        body,
    });
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    StatusCode::from_u16(status).unwrap()
}

/// A port nothing listens on, to be listened on later.
async fn dead_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
}

// ---- the relay and the game ----

/// The settings for one game server: its token, these targets, and quick retries.
fn settings(targets: &str, delivery: &str) -> String {
    format!(
        "listen = \"127.0.0.1:0\"\nadmin_listen = \"127.0.0.1:0\"\ntoken = \"{FEED_TOKEN}\"\n\
         {targets}\n[delivery]\nmin_backoff = \"20ms\"\nmax_backoff = \"200ms\"\ntimeout = \"5s\"\n{delivery}\n"
    )
}

fn config(targets: &str) -> Config {
    Config::parse(&settings(targets, "")).unwrap()
}

fn target_toml(url: &str, extra: &str) -> String {
    format!("[[target]]\nurl = \"{url}\"\n{extra}\n")
}

fn http() -> reqwest::Client {
    ciws::deliver::use_ring();
    reqwest::Client::new()
}

async fn game_post(relay: &Relay, token: &str, body: &str) -> (u16, String) {
    let resp = http()
        .post(format!("http://{}/api/ingest/events", relay.ingest_addr))
        .header("authorization", format!("Bearer {token}"))
        .header("user-agent", GAME_UA)
        .header("content-type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .unwrap();
    (resp.status().as_u16(), resp.text().await.unwrap())
}

async fn admin_get(relay: &Relay, path: &str) -> String {
    http()
        .get(format!("http://{}{path}", relay.admin_addr))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

async fn status(relay: &Relay) -> serde_json::Value {
    serde_json::from_str(&admin_get(relay, "/status").await).unwrap()
}

/// Polls `/status` until `ok` holds for it, and answers it.
async fn status_until(
    relay: &Relay,
    what: &str,
    ok: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let s = status(relay).await;
        if ok(&s) {
            return s;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {s}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---- tests ----

#[tokio::test]
async fn every_target_gets_the_batch_as_the_game_sent_it() {
    // One game server's feed to the same server twice (two Warcon rows, say), each with its own
    // token, and to a third place that wants a key header and its own path.
    let panel = Target::start().await;
    let stats = Target::start().await;
    let targets = target_toml(&panel.url(), "token = \"wkf_first_row\"")
        + &target_toml(&panel.url(), "token = \"wkf_second_row\"")
        + &target_toml(
            &format!("{}/hooks/kills?site=1", stats.url()),
            "path = \"\"\nheaders = { \"X-Api-Key\" = \"k1\" }",
        );
    let relay = Relay::start(config(&targets), None).await.unwrap();

    let body = batch(1);
    let started = Instant::now();
    let (code, answer) = game_post(&relay, FEED_TOKEN, &body).await;
    assert_eq!((code, answer.as_str()), (200, r#"{"ok":true}"#));
    eventually("every delivery", || {
        panel.got().len() == 2 && stats.got().len() == 1
    })
    .await;
    assert!(started.elapsed() < Duration::from_secs(1), "in and out");

    let mut tokens: Vec<_> = panel
        .got()
        .iter()
        .map(|g| g.header("authorization").unwrap().to_owned())
        .collect();
    tokens.sort();
    assert_eq!(tokens, ["Bearer wkf_first_row", "Bearer wkf_second_row"]);
    for g in panel.got() {
        assert_eq!(
            g.path, "/api/ingest/events",
            "the game's own path, like [WDServerFeed] Url"
        );
        assert_eq!(g.body, body.as_bytes(), "the body byte for byte");
        assert_eq!(g.header("user-agent"), Some(GAME_UA));
        assert_eq!(g.header("content-type"), Some("application/json"));
        assert_eq!(g.header("via"), Some("1.1 ciws"));
        let at = ciws::deliver::parse_rfc3339_ms(g.header("x-ciws-received-at").unwrap()).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(
            now.abs_diff(at) < 10_000,
            "received-at is the relay's clock"
        );
    }

    let s = &stats.got()[0];
    assert_eq!(
        s.path, "/hooks/kills?site=1",
        "path = \"\" posts to the url as it is"
    );
    assert_eq!(s.body, body.as_bytes());
    assert_eq!(s.header("authorization"), None, "no token, no bearer");
    assert_eq!(s.header("x-api-key"), Some("k1"));

    // A post without the game's User-Agent goes out with CIWS's own.
    let resp = http()
        .post(format!("http://{}/api/ingest/events", relay.ingest_addr))
        .header("authorization", format!("Bearer {FEED_TOKEN}"))
        .body(batch(2))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    eventually("the second batch", || panel.got().len() == 4).await;
    assert_eq!(
        panel.got()[3].header("user-agent"),
        Some(ciws::deliver::FALLBACK_USER_AGENT)
    );

    let s = status(&relay).await;
    let names: Vec<_> = s["feeds"][0]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    let host = panel.addr.ip().to_string();
    assert_eq!(names[0], format!("{host}-{}", panel.addr.port()));
    assert_eq!(names[1], format!("{host}-{}-2", panel.addr.port()));
    let metrics = admin_get(&relay, "/metrics").await;
    assert!(
        metrics.contains("ciws_posts_total{outcome=\"accepted\"} 2"),
        "{metrics}"
    );
    assert!(metrics.contains("ciws_events_total 2"));
    relay.shutdown().await;
}

#[tokio::test]
async fn what_is_not_a_feed_batch_is_refused() {
    let t = Target::start().await;
    let relay = Relay::start(config(&target_toml(&t.url(), "")), None)
        .await
        .unwrap();
    let client = http();
    let url = format!("http://{}/api/ingest/events", relay.ingest_addr);

    assert_eq!(
        game_post(&relay, "cws_not_a_token_of_this_relay_at_all", &batch(1))
            .await
            .0,
        401
    );
    let no_auth = client.post(&url).body(batch(1)).send().await.unwrap();
    assert_eq!(no_auth.status(), 401);
    let basic = client
        .post(&url)
        .header("authorization", "Basic abc")
        .body(batch(1))
        .send()
        .await
        .unwrap();
    assert_eq!(basic.status(), 401);

    let (status, answer) = game_post(&relay, FEED_TOKEN, "[1,2]").await;
    assert_eq!(status, 400);
    assert!(answer.contains("not_a_batch"), "{answer}");
    assert_eq!(game_post(&relay, FEED_TOKEN, "{\"events\":").await.0, 400);
    let many = format!("{{\"events\":[{}]}}", vec!["{}"; 201].join(","));
    let (status, answer) = game_post(&relay, FEED_TOKEN, &many).await;
    assert_eq!((status, answer.contains("too_many_events")), (400, true));
    let huge = format!("{{\"events\":[],\"pad\":\"{}\"}}", "x".repeat(70_000));
    assert_eq!(game_post(&relay, FEED_TOKEN, &huge).await.0, 413);

    let looped = client
        .post(&url)
        .header("authorization", format!("Bearer {FEED_TOKEN}"))
        .header("via", "1.1 ciws, 1.1 ciws, 1.1 ciws, 1.1 ciws")
        .body(batch(1))
        .send()
        .await
        .unwrap();
    assert_eq!(looped.status(), 508);
    let one_hop = client
        .post(&url)
        .header("authorization", format!("Bearer {FEED_TOKEN}"))
        .header("via", "1.1 ciws")
        .body(batch(9))
        .send()
        .await
        .unwrap();
    assert_eq!(
        one_hop.status(),
        200,
        "a relay in front of this one is fine"
    );
    eventually("the one good batch", || t.got().len() == 1).await;
    assert_eq!(t.got()[0].header("via"), Some("1.1 ciws, 1.1 ciws"));

    let metrics = admin_get(&relay, "/metrics").await;
    for line in [
        "ciws_posts_total{outcome=\"unauthorized\"} 3",
        "ciws_posts_total{outcome=\"not_a_batch\"} 3",
        "ciws_posts_total{outcome=\"too_large\"} 1",
        "ciws_posts_total{outcome=\"looped\"} 1",
        "ciws_posts_total{outcome=\"accepted\"} 1",
    ] {
        assert!(metrics.contains(line), "missing {line}\n{metrics}");
    }
    relay.shutdown().await;
}

#[tokio::test]
async fn a_failing_target_catches_up_in_order_and_holds_up_nobody() {
    let flaky = Target::start().await;
    let healthy = Target::start().await;
    let slow = Target::start().await;
    flaky.script(&[(503, 0), (502, 0), (429, 0), (500, 0)]);
    slow.script(&[(200, 3000)]);
    let targets = target_toml(&flaky.url(), "name = \"flaky\"")
        + &target_toml(&healthy.url(), "name = \"healthy\"")
        + &target_toml(&slow.url(), "name = \"slow\"");
    let relay = Relay::start(config(&targets), None).await.unwrap();

    let sent: Vec<String> = (1..=5).map(batch).collect();
    for body in &sent {
        let started = Instant::now();
        assert_eq!(game_post(&relay, FEED_TOKEN, body).await.0, 200);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "the game is never kept waiting by a target"
        );
    }
    eventually("the healthy target", || healthy.got().len() == 5).await;
    assert_eq!(healthy.bodies(), sent);
    assert!(
        slow.got().len() <= 1,
        "the slow target is still on its first batch"
    );

    eventually("the flaky target to catch up", || flaky.bodies().len() == 9).await;
    assert_eq!(
        flaky.bodies()[4..].to_vec(),
        sent,
        "after four failures, all five in order"
    );
    assert!(
        flaky.bodies()[..4].iter().all(|b| *b == sent[0]),
        "the first batch is the one retried"
    );
    eventually("the slow target", || slow.got().len() == 5).await;
    assert_eq!(slow.bodies(), sent);

    let s = status_until(&relay, "the flaky target's counters", |s| {
        s["feeds"][0]["targets"][0]["delivered"] == 5
    })
    .await;
    let flaky_status = &s["feeds"][0]["targets"][0];
    assert_eq!(flaky_status["name"], "flaky");
    assert_eq!(flaky_status["failed_attempts"], 4);
    assert_eq!(flaky_status["state"], "ok");
    relay.shutdown().await;
}

#[tokio::test]
async fn a_target_that_was_down_gets_what_it_missed_and_a_full_queue_drops_the_oldest() {
    let port = dead_port().await;
    let text = settings(
        &target_toml(&format!("http://{port}"), "token = \"wkf_t\""),
        "queue = 3",
    );
    let relay = Relay::start(Config::parse(&text).unwrap(), None)
        .await
        .unwrap();
    assert_eq!(game_post(&relay, FEED_TOKEN, &batch(1)).await.0, 200);
    // Batch 1 is in the lane's hands, retried while the target cannot be reached.
    let s = status_until(&relay, "the target to be failing", |s| {
        s["feeds"][0]["targets"][0]["state"] == "failing"
    })
    .await;
    assert_eq!(
        s["feeds"][0]["targets"][0]["last_error"],
        "could not connect"
    );
    for n in 2..=5 {
        assert_eq!(game_post(&relay, FEED_TOKEN, &batch(n)).await.0, 200);
    }
    // Queued: 3, 4, 5. Batch 2 made way for batch 5.
    let s = status(&relay).await;
    assert_eq!(s["feeds"][0]["targets"][0]["backlog"], 4);
    assert_eq!(s["feeds"][0]["targets"][0]["dropped"], 1);

    let target = Target::on(TcpListener::bind(port).await.unwrap()).await;
    eventually("what was kept", || target.got().len() == 4).await;
    assert_eq!(target.bodies(), [batch(1), batch(3), batch(4), batch(5)]);
    assert!(
        target
            .got()
            .iter()
            .all(|g| g.header("authorization") == Some("Bearer wkf_t"))
    );
    let s = status_until(&relay, "the backlog to clear", |s| {
        s["feeds"][0]["targets"][0]["backlog"] == 0
    })
    .await;
    assert_eq!(s["feeds"][0]["targets"][0]["delivered"], 4);
    let metrics = admin_get(&relay, "/metrics").await;
    assert!(metrics.contains("outcome=\"dropped\"} 1"), "{metrics}");
    relay.shutdown().await;
}

#[tokio::test]
async fn a_batch_the_target_refuses_is_not_sent_again() {
    let t = Target::start().await;
    t.script(&[(400, 0)]);
    let relay = Relay::start(config(&target_toml(&t.url(), "")), None)
        .await
        .unwrap();
    game_post(&relay, FEED_TOKEN, &batch(1)).await;
    game_post(&relay, FEED_TOKEN, &batch(2)).await;
    eventually("both batches tried", || t.got().len() == 2).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        t.bodies(),
        [batch(1), batch(2)],
        "the refused batch was not retried"
    );
    let s = status_until(&relay, "both batches counted", |s| {
        s["feeds"][0]["targets"][0]["delivered"] == 1
    })
    .await;
    let target = &s["feeds"][0]["targets"][0];
    assert_eq!(target["refused"], 1);
    assert_eq!(target["backlog"], 0);
    relay.shutdown().await;
}

#[tokio::test]
async fn max_age_lets_a_live_target_skip_what_went_stale() {
    let port = dead_port().await;
    let relay = Relay::start(
        config(&target_toml(&format!("http://{port}"), "max_age = \"1s\"")),
        None,
    )
    .await
    .unwrap();
    game_post(&relay, FEED_TOKEN, &batch(1)).await;
    game_post(&relay, FEED_TOKEN, &batch(2)).await;
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let target = Target::on(TcpListener::bind(port).await.unwrap()).await;
    game_post(&relay, FEED_TOKEN, &batch(3)).await;
    eventually("the fresh batch", || target.got().len() == 1).await;
    assert_eq!(target.bodies(), [batch(3)]);
    let s = status(&relay).await;
    assert_eq!(s["feeds"][0]["targets"][0]["expired"], 2);
    relay.shutdown().await;
}

#[tokio::test]
async fn saving_the_settings_file_applies_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ciws.toml");
    let first = Target::start().await;
    let second = Target::start().await;
    std::fs::write(
        &path,
        settings(&target_toml(&first.url(), "name = \"first\""), ""),
    )
    .unwrap();
    let relay = Relay::start(Config::load(&path).unwrap(), Some(path.clone()))
        .await
        .unwrap();
    game_post(&relay, FEED_TOKEN, &batch(1)).await;
    eventually("the first target", || first.got().len() == 1).await;

    // A second target, saved while running.
    let both = target_toml(&first.url(), "name = \"first\"")
        + &target_toml(&second.url(), "name = \"second\"");
    std::fs::write(&path, settings(&both, "")).unwrap();
    status_until(&relay, "the edit to be applied", |s| {
        s["feeds"][0]["targets"].as_array().unwrap().len() == 2
    })
    .await;
    game_post(&relay, FEED_TOKEN, &batch(2)).await;
    eventually("both targets", || {
        first.got().len() == 2 && second.got().len() == 1
    })
    .await;
    assert_eq!(
        second.bodies(),
        [batch(2)],
        "a new target starts from the edit, not the history"
    );

    // A file with a mistake is reported, and what was running carries on.
    std::fs::write(&path, "token = \"short\"\n").unwrap();
    tokio::time::sleep(Duration::from_millis(2700)).await;
    game_post(&relay, FEED_TOKEN, &batch(3)).await;
    eventually("delivery on the old settings", || {
        first.got().len() == 3 && second.got().len() == 2
    })
    .await;
    relay.shutdown().await;
}

#[tokio::test]
async fn a_reload_starts_new_targets_and_drops_removed_ones() {
    let port = dead_port().await;
    let kept = Target::start().await;
    let added = Target::start().await;
    let before = target_toml(&kept.url(), "name = \"kept\"")
        + &target_toml(&format!("http://{port}"), "name = \"removed\"");
    let relay = Relay::start(config(&before), None).await.unwrap();
    game_post(&relay, FEED_TOKEN, &batch(1)).await;
    game_post(&relay, FEED_TOKEN, &batch(2)).await;
    eventually("the kept target", || kept.got().len() == 2).await;

    let after = target_toml(&kept.url(), "name = \"kept\"")
        + &target_toml(&added.url(), "name = \"added\"");
    relay.reload(config(&after)).await;
    game_post(&relay, FEED_TOKEN, &batch(3)).await;
    eventually("both targets", || {
        kept.got().len() == 3 && added.got().len() == 1
    })
    .await;
    assert_eq!(added.bodies(), [batch(3)]);

    let s = status(&relay).await;
    let names: Vec<&str> = s["feeds"][0]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["kept", "added"]);
    let metrics = admin_get(&relay, "/metrics").await;
    assert!(!metrics.contains("target=\"removed\""));
    // What was queued for the removed target went with it, so stopping does not wait for it.
    let started = Instant::now();
    relay.shutdown().await;
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn a_target_that_loops_back_is_cut_off_by_the_hop_limit() {
    let t = Target::start().await;
    let relay = Relay::start(config(&target_toml(&t.url(), "name = \"t\"")), None)
        .await
        .unwrap();
    // The relay's own address is only known once it listens; a reload points a target at it.
    let looped = target_toml(&t.url(), "name = \"t\"")
        + &target_toml(
            &format!("http://{}", relay.ingest_addr),
            &format!("name = \"self\"\ntoken = \"{FEED_TOKEN}\""),
        );
    relay.reload(config(&looped)).await;

    assert_eq!(game_post(&relay, FEED_TOKEN, &batch(1)).await.0, 200);
    // The game's post and three relayed copies get in; the fourth hop is refused with 508.
    let s = status_until(&relay, "the loop to hit the limit", |s| {
        s["feeds"][0]["targets"][1]["last_status"] == 508
    })
    .await;
    assert_eq!(s["feeds"][0]["targets"][1]["state"], "failing");
    eventually("the copies that got in", || t.got().len() == 4).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(t.got().len(), 4, "no more than the hop limit allows");
    let vias: Vec<_> = t
        .got()
        .iter()
        .map(|g| g.header("via").unwrap().matches("ciws").count())
        .collect();
    assert_eq!(vias, [1, 2, 3, 4]);
    relay.shutdown().await;
}

#[tokio::test]
async fn stopping_sends_what_is_queued_first() {
    let t = Target::start().await;
    t.script(&[(200, 150), (200, 150), (200, 150)]);
    let relay = Relay::start(config(&target_toml(&t.url(), "")), None)
        .await
        .unwrap();
    for n in 1..=3 {
        assert_eq!(game_post(&relay, FEED_TOKEN, &batch(n)).await.0, 200);
    }
    relay.shutdown().await;
    assert_eq!(t.bodies(), [batch(1), batch(2), batch(3)]);
}

#[tokio::test]
async fn a_feed_with_no_targets_still_answers_the_game() {
    let relay = Relay::start(config(""), None).await.unwrap();
    assert_eq!(game_post(&relay, FEED_TOKEN, &batch(1)).await.0, 200);
    let health = http()
        .get(format!("http://{}/healthz", relay.ingest_addr))
        .send()
        .await
        .unwrap();
    assert_eq!(health.text().await.unwrap(), "ok");
    relay.shutdown().await;
}

#[tokio::test]
async fn in_and_out_on_the_game_s_own_path_only() {
    let t = Target::start().await;
    let relay = Relay::start(config(&target_toml(&t.url(), "")), None)
        .await
        .unwrap();
    let post = |path: &'static str| {
        http()
            .post(format!("http://{}{path}", relay.ingest_addr))
            .header("authorization", format!("Bearer {FEED_TOKEN}"))
            .body(batch(1))
            .send()
    };
    assert_eq!(post("/api/ingest/events").await.unwrap().status(), 200);
    // What the game posts to when its Url carries a path (…:7780/, …:7780/ciws, the whole
    // ingest address): refused, and counted as the game's post on the wrong path.
    for path in [
        "//api/ingest/events",
        "/ciws/api/ingest/events",
        "/api/ingest/events/api/ingest/events",
    ] {
        assert_eq!(post(path).await.unwrap().status(), 404, "{path}");
    }
    for path in ["/", "/api/ingest", "/api/ingest/events/"] {
        assert_eq!(post(path).await.unwrap().status(), 404, "{path}");
    }
    let get = http()
        .get(format!("http://{}/api/ingest/events", relay.ingest_addr))
        .send()
        .await
        .unwrap();
    assert_eq!(get.status(), 405);

    eventually("the one accepted batch", || t.got().len() == 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(t.got().len(), 1);
    assert_eq!(
        t.got()[0].path,
        "/api/ingest/events",
        "out on the game's path too"
    );
    let metrics = admin_get(&relay, "/metrics").await;
    assert!(
        metrics.contains("ciws_posts_total{outcome=\"wrong_path\"} 3"),
        "{metrics}"
    );
    assert!(metrics.contains("ciws_posts_total{outcome=\"accepted\"} 1"));
    relay.shutdown().await;
}
