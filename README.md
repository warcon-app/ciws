<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="branding/ciws-logo-on-dark.svg">
    <img src="branding/ciws-logo-on-light.svg" alt="CIWS" height="72">
  </picture>
</p>

# CIWS

Your WARDOGS server can send its kill feed to one place. CIWS sends it to as many as you like.

A WARDOGS dedicated server pushes every kill to the one address in `[WDServerFeed] Url`, with the
one `Token`. Point it at [Warcon](https://warcon.app) and your stats site goes without; point it
at the stats site and Warcon does. Point it at CIWS instead, and CIWS passes every batch on to
each place you list, each with its own token.

- **Simple.** One small Docker container and one short settings file, which it writes for you. No
  database, nothing to look after.
- **In and out.** The game gets its answer in well under a millisecond, and every place gets the
  batch a moment later.
- **One place being down does not affect the others.** Each place has its own queue. One that is
  slow or down for a while gets what it missed, in order, when it is back; the rest carry on.
- **Nothing to change at the other end.** Each place gets exactly what the game would have sent
  it, with its own token. Warcon takes it as it is.

## Contents

- [Set it up](#set-it-up)
- [Without Docker](#without-docker)
- [HTTPS](#https)
- [Using it with Warcon](#using-it-with-warcon)
- [More places, more servers](#more-places-more-servers)
- [When a place is down](#when-a-place-is-down)
- [Checking on it](#checking-on-it)
- [Settings](#settings)
- [What a place receives](#what-a-place-receives)
- [Security](#security)
- [Performance](#performance)
- [Building it yourself](#building-it-yourself)

## Set it up

With [Docker](https://docs.docker.com/engine/install/), on the game server's machine or any
machine the game server can reach:

1. **Make a folder for CIWS** with this `compose.yaml` in it (it is also
   [in this repository](compose.yaml)):

   ```yaml
   services:
     ciws:
       image: ghcr.io/warcon-app/ciws:latest
       restart: unless-stopped
       volumes:
         - .:/config:ro
       ports:
         - "7780:7780"
         - "127.0.0.1:7781:7781"
   ```

2. **Make its settings file**, with a new token, in that folder:

   ```sh
   docker run --rm ghcr.io/warcon-app/ciws init > ciws.toml
   ```

   It shows the lines your game server needs:

   ```ini
   [WDServerFeed]
   Url=http://127.0.0.1:7780
   Token=cws_…
   ```

   Add them to the game server's `ServerSettings.ini` and restart the game server: it reads these
   lines only when it starts. `Url` is the address alone, with nothing after the port: the game
   adds `/api/ingest/events` itself, and that is the one path CIWS takes it on. If the game server
   is on another machine, or runs in Docker itself, put the address of the machine CIWS runs on in
   place of `127.0.0.1`; across the internet, use an `https://` address, with a reverse proxy in
   front of CIWS ([HTTPS](#https)).

3. **Add the places the kill feed should go.** Open `ciws.toml`, add a `[[target]]` for each,
   and save:

   ```toml
   [[target]]
   url = "https://console.warcon.app"
   token = "wkf_…"
   ```

4. **Start it:**

   ```sh
   docker compose up -d
   docker compose logs -f
   ```

   Once the game server is back up and someone dies, the log says `the kill feed is arriving`.
   From then on, saving `ciws.toml` applies it within a couple of seconds, without a restart, and
   Docker starts CIWS again after a reboot.

To update CIWS: `docker compose pull && docker compose up -d`.

## Without Docker

CIWS is also one file for each system on the [releases page](../../releases):
`ciws-windows-x86_64.exe`, `ciws-linux-x86_64`, `ciws-linux-arm64` and `ciws-macos-arm64`. Put it
in a folder of its own and run it. On Windows, double-click it; if Windows says it protected your
PC from an unrecognised app, choose _More info_, then _Run anyway_. On Linux:

```sh
chmod +x ciws-linux-x86_64
./ciws-linux-x86_64
```

(On a Mac, allow it under System Settings, Privacy & Security, the first time.)

The first time, it makes `ciws.toml` next to itself and shows the game server's lines, as in step
2 above; carry on from step 3. Then leave it running. On Windows, leave its window open, or have
Task Scheduler start it: a task that runs `ciws-windows-x86_64.exe` at startup, whether or not
anyone is logged on, with _Start in_ set to its folder. On Linux, with systemd and CIWS in
`/opt/ciws`, owned by the account that will run it:

```ini
# /etc/systemd/system/ciws.service
[Unit]
Description=CIWS, the WARDOGS kill feed relay
After=network-online.target
Wants=network-online.target

[Service]
User=steam
WorkingDirectory=/opt/ciws
ExecStart=/opt/ciws/ciws-linux-x86_64
Restart=always

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl enable --now ciws
journalctl -u ciws -f          # what it is doing
```

## HTTPS

CIWS speaks plain HTTP. On the game server's own machine, or across a private network, that is
all it needs: `Url=http://127.0.0.1:7780`, or the other machine's address.

When the game server reaches CIWS across the internet, give CIWS a domain name and put a reverse
proxy with a certificate in front of it, then set `Url=https://feed.example.com` (your domain, with
nothing after it). The proxy has to pass `/api/ingest/events` through as it is: CIWS takes the
game's posts on that path only.

With Docker, [Caddy](https://caddyserver.com) does it in one more service, and gets and renews the
certificate by itself. Point the domain's DNS at the machine, keep ports 80 and 443 open to it,
and use this `compose.yaml` instead, so only Caddy faces the internet:

```yaml
services:
  ciws:
    image: ghcr.io/warcon-app/ciws:latest
    restart: unless-stopped
    volumes:
      - .:/config:ro
    ports:
      - "127.0.0.1:7781:7781"
  caddy:
    image: caddy:2
    restart: unless-stopped
    command: caddy reverse-proxy --from feed.example.com --to ciws:7780
    ports:
      - "80:80"
      - "443:443"
    volumes:
      - caddy-data:/data
volumes:
  caddy-data:
```

Without Docker, the same in a Caddyfile:

```
feed.example.com {
	reverse_proxy 127.0.0.1:7780
}
```

nginx, Traefik or anything else that terminates TLS works the same way. Behind Cloudflare, the
game's posts pass through a proxied domain as they are; keep any firewall or bot rule from
challenging them, since the game's User-Agent is not a browser's.

## Using it with Warcon

In Warcon, each server's **Config** tab has a **Kill feed** card. **Configure** there makes a
`wkf_…` token and writes Warcon's own address into the game's config. To put CIWS in between:

1. Click **Configure** in Warcon, if you have not already, and copy the token it shows.
2. Add Warcon to `ciws.toml` with that token, as in step 3 above.
3. Set the game server's `[WDServerFeed]` to CIWS's `Url` and `Token`, as in step 2, and restart
   the game server.

To change only one line instead: keep `Token=wkf_…` as Warcon wrote it, change just `Url`, and
use the same `wkf_…` token as the `token` at the top of `ciws.toml`. Taking CIWS out again is the
same one line.

Clicking **Configure** in Warcon again writes Warcon's address back into `Url`, which takes the
feed off CIWS at the game server's next restart.

## More places, more servers

**Several places**: one `[[target]]` each. The same address can appear more than once with
different tokens: one game server added to Warcon by two organisations, say, each with its own
`wkf_` token.

**A place with its own path, or a key header**:

```toml
[[target]]
url = "https://stats.example.com/hooks/wardogs"
path = ""                         # post to url exactly as written
headers = { "X-Api-Key" = "…" }
```

**More game servers**: a `[[feed]]` for each, with its own token (what that server's
`[WDServerFeed] Token` is set to; `ciws token` makes one) and its own targets:

```toml
[[feed]]
name = "server-2"
token = "…"

  [[feed.target]]
  url = "https://console.warcon.app"
  token = "wkf_…"
```

[`ciws.example.toml`](ciws.example.toml) has every setting.

## When a place is down

- Nothing touches the disk. A batch goes on each place's queue, the game gets its answer at once,
  and each place then gets the batches in the order the game sent them, one at a time.
- A place that does not answer, or answers with an error, is tried again after 1 s, 2 s, 4 s and
  so on, up to every five minutes, while its queue holds what comes in meanwhile. When it answers
  again it gets them all, in order. The other places are never held up.
- A queue holds 1,000 batches: about two hours of a busy server. When it is full, the oldest are
  dropped to make room. `[delivery] queue` changes that.
- Stopping CIWS gives the queues a few seconds to empty, then drops what is left. A restart of
  CIWS loses whatever was still waiting.
- A place that answers `400`, `413` or `422` has refused that batch: it is not sent there again.
  A `401` or `404` usually means a wrong token or address: it is retried, so fix it in
  `ciws.toml` and save. Redirects are not followed.
- `max_age` is for a place that acts on kills as they happen (Warcon's team-kill rules do): it
  skips batches older than that, rather than deliver them late.
- Now and then a place can get a batch twice (when its answer was lost on the way back). Every
  event has an `eventId` to recognise a repeat by, as Warcon does.

## Checking on it

Its log (`docker compose logs -f`, or its window, or `journalctl -u ciws`) says what matters: when
the kill feed starts arriving, where it goes, a place that stops answering and when it is back,
and any mistake in `ciws.toml`, which it reports without stopping:

```
2026-10-09T18:02:11Z  INFO listening for the game on 0.0.0.0:7780; status at http://127.0.0.1:7781/status
2026-10-09T18:02:11Z  INFO the kill feed goes to console.warcon.app, stats (stats.example.com)
2026-10-09T18:04:37Z  INFO the kill feed is arriving
2026-10-09T19:15:02Z  WARN stats (stats.example.com) answered 502; trying again in 1s
2026-10-09T19:15:44Z  INFO stats (stats.example.com): delivering again
```

On the machine it runs on, <http://127.0.0.1:7781/status> shows each place: `ok`, `behind` or
`failing`, how many batches are waiting, how many were delivered, refused, expired or dropped,
and its last answer. `/metrics` has the same for Prometheus (`ciws_posts_total`,
`ciws_deliveries_total{feed,target,outcome}`, `ciws_delivery_backlog`,
`ciws_delivery_failures_total`, …).

## Settings

`ciws.toml`. A running CIWS applies it whenever it is saved. Any value may use `${ENV_VAR}`.

| Setting | Default | |
| --- | --- | --- |
| `token` | | what the game sends as `[WDServerFeed] Token`: 16 to 512 characters |
| `[[target]]` `url` | | `http://` or `https://`: the address alone, as for the game's `Url` |
| `[[target]]` `token` | none | sent as `Authorization: Bearer <token>` |
| `[[target]]` `name` | from the host | how logs and `/status` call it |
| `[[target]]` `path` | `/api/ingest/events` | added to `url`; `""` posts to `url` exactly as written |
| `[[target]]` `headers` | none | e.g. `{ "X-Api-Key" = "…" }` |
| `[[target]]` `max_age` | none | skip batches older than this, e.g. `"10m"` |
| `[[feed]]` `name`, `token`, `[[feed.target]]` | | one more game server |
| `listen` | `0.0.0.0:7780` | where the game posts (`CIWS_LISTEN` overrides it) |
| `admin_listen` | `127.0.0.1:7781` | `/status`, `/metrics` (`CIWS_ADMIN_LISTEN` overrides it) |
| `[delivery]` `queue` | `1000` | batches each place may have waiting |
| `[delivery]` `timeout` | `10s` | per attempt |
| `[delivery]` `min_backoff`, `max_backoff` | `1s`, `5m` | the waits between attempts |
| `[delivery]` `block_private` | `false` | refuse places on private, loopback or link-local addresses |

`listen`, `admin_listen` and `block_private` change at the next start; everything else when the
file is saved.

## What a place receives

What the game would have sent it, with two headers added:

| | |
| --- | --- |
| `POST <url>/api/ingest/events` | the game's own path; `<url><path>` when the target sets `path` |
| body | exactly the game's: `{ "serverId", "serverName", "events": [ … ] }` |
| `Authorization` | `Bearer <token>`, when the target has a token |
| `User-Agent` | the game's own, e.g. `Wardogs/++Wardogs+Live-CL-507060 (http-eventloop) Linux/debian12` |
| `Content-Type` | `application/json` |
| `Via` | `1.1 ciws` for each relay the batch passed through; the fifth is refused, to stop loops |
| `X-Ciws-Received-At` | when CIWS took the batch, e.g. `2026-10-09T18:04:37.120Z` |

The game's events carry only the match clock, so a place that stamps kills with its own clock
puts a late delivery at the wrong time; `X-Ciws-Received-At` is the time to use. Warcon's
[notes on the WARDOGS API](https://github.com/warcon-app/warcon/blob/main/docs/wardogs-api.md)
describe every field of the game's events.

## Security

- The token at the top of `ciws.toml` is the only thing that lets a game server in: anyone who has
  it can send kills to your places, so keep the folder to yourself. (Run without Docker, CIWS
  makes the file readable by its owner only; with Docker the container has to be able to read it.)
- Tokens are never logged or shown, and neither is a place's full address (some, like a Discord
  webhook's, carry a secret): logs and `/status` show only its host. What a place answers is never
  kept or shown, only its status code.
- `/status` and `/metrics` listen on `127.0.0.1` only, unless you change `admin_listen`.
- `block_private = true` is for a CIWS whose places other people choose: none of them may then be
  a private, loopback, link-local or otherwise internal address, checked when a name is looked up
  as well as when it is written down.
- Report a vulnerability privately with GitHub's "Report a vulnerability" button, not in an issue.

## Performance

A busy 100-player server posts about 550 times an hour, a batch of one to ten kills each time.
There are around 3,000 WARDOGS servers, most of them official ones run by the developer, so every
community server at once is a few hundred posts a second.

`examples/loadtest.rs` plays game servers and places against a real CIWS. With CIWS as its own
process, built as the Docker image's is (static Linux) and run in Docker on an Apple M1 Max:

| Load | CIWS's CPU | CIWS's memory | Game's answer, median / 99th percentile |
| --- | --- | --- | --- |
| one server, three places | idle | 13 to 18 MB | |
| 1,500 servers × 3 places, each posting as a busy server does | 3% of one core | 86 MB | 0.17 ms / 0.9 ms |
| the same at three times that rate | 9% of one core | 101 MB | 0.16 ms / 2.4 ms |
| 2,000 servers × 3 places, 27,000 posts a second | 2.1 cores | 140 MB | 1.1 ms / 5 to 13 ms |

The last row is about 80,000 deliveries a second, with every place caught up within 0.1 s of the
last post. The 99th percentile moves between about 0.5 and 4 ms from run to run at the lighter
loads. A typical batch waiting on a queue takes 1 to 2 KB.

```sh
cargo run --release --example loadtest -- --feeds 1500 --rate 0.5 --relay-bin target/release/ciws
```

## Building it yourself

With [Rust](https://rustup.rs):

```sh
cargo build --release        # target/release/ciws
cargo test                   # unit tests, and tests/relay.rs end to end with scripted places
```

```
src/main.rs      the command line: run, init, check, token
src/config.rs    ciws.toml
src/relay.rs     starting, applying a saved ciws.toml, stopping
src/ingest.rs    the address the game posts to
src/wire.rs      the one check made on a body: a JSON object with an events array
src/queue.rs     a place's queue, in memory
src/lanes.rs     one task per place: deliver in order, wait and try again when it fails
src/deliver.rs   one POST, and what its answer means
src/routes.rs    the routing table, swapped whole when ciws.toml changes
src/netguard.rs  block_private
src/admin.rs     /status and /metrics
branding/        the logo; branding/src/generate.ts makes it
```

## Licence

MIT
