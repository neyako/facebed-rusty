# facebed

A Facebook embed provider for Discord and other messaging apps.

[![2IrN8Ux.png](https://iili.io/2IrN8Ux.png)](https://freeimage.host/)

## For users

Replace `www.facebook.com` in your Facebook URL with `facebed.com`.

<details>
<summary>Vencord settings</summary>

Using regex:
- Find: `https://(www.)?facebook.com/(.*)`
- Replace: `https://facebed.com/$2`

</details>

Supported URLs: posts, group posts, photos, photo comments, reels, watch videos, **24-hour stories**, and mobile share links (`/share/r/`, `/share/p/`, `/share/v/`).

---

# For developers and maintainers

facebed is a Rust HTTP server (axum + reqwest + scraper). It scrapes Facebook's HTML, pulls
OpenGraph data out of the embedded JSON blobs, and serves a minimal `<meta>`-only page back to
crawlers. Humans get a 301 to Facebook.

## Deploy with Docker (recommended)

```bash
# 1. Copy example config and cookies
cp config.example.yaml config.yaml
cp cookies.example.json cookies.json   # optional but recommended

# 2. Edit them
$EDITOR config.yaml
$EDITOR cookies.json

# 3. Build & run
docker compose up -d --build
```

The image is built `FROM scratch` with a statically-linked musl binary. Final size ~10 MB,
no shell, no libc surface. Container runs as uid 65532 (non-root).

To rebuild on a code change:

```bash
docker compose up -d --build
```

To follow logs:

```bash
docker compose logs -f
```

Put a reverse proxy (nginx, Caddy, Traefik) in front of it for TLS termination. The binary
serves plain HTTP on the configured port.

## Cookies

facebed can fetch private content (friends-only posts, private groups, viewable stories) when
authenticated. Cookies are loaded from `./cookies.json` AND any sibling file matching
`cookies*.json` (e.g. `cookies-alice.json`, `cookies2.json`, `cookies-neyako.json`). Each file
contributes one or more accounts to a round-robin pool; the account label is taken from the
filename when it isn't supplied in the file itself.

Two file shapes are accepted:

1. **Flat Cookie-Editor export** — a JSON array of `{name, value, expirationDate, ...}`. Becomes
   one account; the label is the part of the filename after `cookies` (so `cookies-alice.json`
   → `alice`, plain `cookies.json` → `default`).
2. **Multi-account object** — `{"accounts": [{"label": "...", "entries": [...]}, ...]}`. Each
   listed account is loaded with its declared label.

Easiest way to add another account: drop the Cookie-Editor export as
`cookies-<name>.json` next to the existing `cookies.json`. No config edit needed. When running
in Docker, also add a matching volume mount in `docker-compose.yml` (commented examples are
included).

When any cookie is past its `expirationDate`, facebed posts a `@everyone cookies expired` alert
to the Discord webhook configured in `notifier_webhook`. The same webhook also fires after an
account fails 3 fetches in a row (cookie likely checkpointed or invalidated mid-run), with the
account label and last error attached so you know which file to re-export.

### Per-account user agents (optional)

Drop a `useragents.json` next to your cookies to make each account send a distinct UA — so a
multi-account pool looks like several different browsers / devices rather than one scraper
behind one Chrome UA. The keys are account labels (matching the `cookies-<label>.json`
filename convention):

```json
{
  "alice": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 ...",
  "bob":   "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 ..."
}
```

Missing labels fall back to a default Chrome UA. See `useragents.example.json`.

> [!WARNING]
> Fetching with cookies will hit your account's rate limits and may trigger a Facebook
> security checkpoint if the account does anything Facebook considers unusual. Use a
> dedicated account (or a few of them) for this — not your primary.

## Local development (without Docker)

Requires Rust 1.75+.

```bash
cargo run -- -c config.yaml
```

Or release build:

```bash
cargo build --release
./target/release/facebed -c config.yaml --cookies cookies.json
```

Pass `RUST_LOG=debug` for verbose tracing.

## Config schema

See `config.example.yaml`. All keys have defaults — `cargo run` with no args works.

| Key | Default | Notes |
|---|---|---|
| `host` | `0.0.0.0` | Bind address. |
| `port` | `9812` | TCP port. |
| `timezone` | `7` | Hours offset from UTC for embed timestamps. |
| `banned_users` | `[]` | FB author IDs whose posts return a placeholder instead of being embedded. |
| `notifier_webhook` | `""` | Discord webhook URL for parser-bug and cookie-expiry alerts. |

## Project not affiliated with Meta / Facebook.
