# facebed-rusty

Self-hosted Facebook OpenGraph embed proxy for Discord, Slack, Telegram, and other messaging
apps.

![facebed embed preview](assets/readme-preview.png)

Facebook often gives chat apps a plain login wall instead of a useful link preview. facebed
fetches the Facebook page from your server, extracts the post data, and returns a tiny HTML page
with OpenGraph tags. Crawlers see the preview. Normal browser users get redirected back to
Facebook.

This project is not affiliated with Meta or Facebook.

## Why self-host it?

Run your own copy on your own domain, for example `facebed.example.com`, then replace:

```text
https://www.facebook.com/example/posts/123
```

with:

```text
https://facebed.example.com/example/posts/123
```

Self-hosting also lets you provide your own Facebook cookies for posts that are visible to your
account but not visible to an incognito browser, such as private groups, friends-only posts, and
some stories.

## Features

- Works as a small HTTP service behind nginx, Caddy, Traefik, Cloudflare Tunnel, or any reverse
  proxy that can forward HTTPS traffic to a local port.
- Humans are redirected to Facebook with a `301`; crawlers get OG/Twitter meta tags.
- Optional Cookie-Editor JSON support for cookie-viewable posts.
- Multiple cookie accounts via `cookies*.json`, with cooldowns, fallback retries, and per-group or
  per-profile account affinity.
- Discord webhook alerts for parser failures and unhealthy cookie accounts.
- Rust binary, async `axum`/`reqwest` server, and a small `scratch` Docker image.

Supported Facebook link shapes:

- Public and cookie-viewable posts, group posts, `story.php`, and `permalink.php`.
- Single photos: `/photo` and `/photo.php`.
- Image-in-comment links with `?type=3`.
- Reels: `/reel/<id>`.
- Watch and Page video posts: `/watch?v=<id>` and `<page>/videos/.../<id>`.
- 24-hour stories: `/stories/<author_id>/<media_id>`.
- Mobile share links: `/share/r/...`, `/share/p/...`, and `/share/v/...`.

## Quick start with Docker

Prerequisites:

- A Linux server or VPS with Docker and Docker Compose.
- A domain or subdomain pointing at that server.
- Ports `80` and `443` open if you want Discord and other external crawlers to reach it.

Clone and create local config files:

```bash
git clone https://github.com/neyako/facebed-rusty.git
cd facebed-rusty
cp config.example.yaml config.yaml
printf '[]\n' > cookies.json
```

Edit `config.yaml` first. It works as-is for most installs:

```yaml
host: 0.0.0.0
port: 9812
timezone: 7
banned_users: []
notifier_webhook: ""
```

The empty `cookies.json` is enough for public-only testing. Replace it with real Facebook cookies
later if you need private groups, friends-only posts, or stories.

Start the service:

```bash
docker compose up -d --build
docker compose logs -f
```

The app now listens on plain HTTP at `http://127.0.0.1:9812` or `http://SERVER_IP:9812`. Put a
reverse proxy in front of it for HTTPS.

## nginx reverse proxy example

Example setup:

- Your public domain is `facebed.example.com`.
- facebed is running on the same server at `127.0.0.1:9812`.
- You use Let's Encrypt certificates from Certbot.

Install nginx and Certbot on Ubuntu/Debian:

```bash
sudo apt update
sudo apt install nginx certbot python3-certbot-nginx
```

Create `/etc/nginx/sites-available/facebed`:

```nginx
server {
    listen 80;
    server_name facebed.example.com;

    location / {
        proxy_pass http://127.0.0.1:9812;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        proxy_connect_timeout 5s;
        proxy_send_timeout 10s;
        proxy_read_timeout 10s;
    }
}
```

Enable it:

```bash
sudo ln -s /etc/nginx/sites-available/facebed /etc/nginx/sites-enabled/facebed
sudo nginx -t
sudo systemctl reload nginx
```

Get HTTPS:

```bash
sudo certbot --nginx -d facebed.example.com
```

After Certbot finishes, test from outside the server:

```bash
curl -I https://facebed.example.com/
```

For Cloudflare users: set SSL/TLS mode to `Full` or `Full (strict)`. `Flexible` can create redirect
loops because nginx sees HTTP from Cloudflare while users see HTTPS.

## Using it in Discord or Vencord

Once your domain works, replace Facebook links manually:

```text
https://www.facebook.com/example/posts/123
https://facebed.example.com/example/posts/123
```

Vencord regex replacement:

- Find: `https://(?:www\.)?facebook\.com/(.*)`
- Replace: `https://facebed.example.com/$1`

Use your own domain in the replacement string.

## Cookies

Cookies are optional for fully public posts. You need cookies for private groups, friends-only
posts, restricted stories, and any page Facebook only shows to a logged-in account.

Basic Cookie-Editor export:

```json
[
  {"name": "c_user", "value": "100000000000001", "domain": ".facebook.com", "expirationDate": 4070908800},
  {"name": "xs", "value": "REPLACE_ME", "domain": ".facebook.com", "expirationDate": 4070908800}
]
```

Save it as `cookies.json` next to `docker-compose.yml`. Do not commit real cookies.

Multi-account options:

- `cookies.json` -> account label `default`
- `cookies-alice.json` -> account label `alice`
- `cookies-bob.json` -> account label `bob`

The server automatically loads sibling files matching `cookies*.json`, except
`cookies.example.json`.

You can also use one multi-account file:

```json
{
  "accounts": [
    {
      "label": "alice",
      "entries": [
        {"name": "c_user", "value": "100000000000001"},
        {"name": "xs", "value": "REPLACE_ME"}
      ]
    }
  ]
}
```

Optional per-account user agents go in `useragents.json`:

```json
{
  "default": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.36",
  "alice": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Safari/605.1.15"
}
```

Warning: cookie-backed scraping can trigger Facebook checkpoints or rate limits. Use a dedicated
account, not your primary personal account.

## Config reference

`config.yaml` is optional when running the binary directly, but the Docker example mounts one for
clarity. Missing keys use defaults.

| Key | Default | Notes |
| --- | --- | --- |
| `host` | `0.0.0.0` | Bind address inside the container or process. |
| `port` | `9812` | HTTP port facebed listens on. |
| `timezone` | `7` | UTC offset for embed timestamps. Valid range: `-12` to `14`. |
| `banned_users` | `[]` | Facebook author IDs that return a placeholder embed. |
| `notifier_webhook` | `""` | Discord webhook for parser bugs and cookie-account health alerts. |

## Verify your install

Check that the service is alive:

```bash
curl http://127.0.0.1:9812/
```

Check crawler output:

```bash
curl -A 'Discordbot/2.0' 'http://127.0.0.1:9812/<facebook-path>'
```

The returned HTML should contain tags like:

```html
<meta property="og:title" content="..."/>
<meta property="og:description" content="..."/>
<meta property="og:image" content="..."/>
```

Check human redirect behavior:

```bash
curl -I 'http://127.0.0.1:9812/<facebook-path>'
```

Without a crawler user agent, you should see a `301` redirect to Facebook.

## Troubleshooting

No Discord preview:

- Make sure your domain is reachable over public HTTPS.
- Use `curl -A 'Discordbot/2.0'` to confirm facebed returns OG tags.
- Discord caches previews. Try a different URL or wait before retesting.
- Some Discord DMs may not show a preview even when the same link embeds in a server.

Error embed codes:

- `C` - no data: login wall, restricted content, expired story, or unsupported URL.
- `P` - parser failure: likely Facebook changed its JSON shape. If a webhook is configured, raw
  HTML is attached for debugging.
- `U` - HTTP, IO, JSON, or YAML failure.
- `X` - unexpected error.
- `T` - Facebook did not finish within the Discord crawler response budget.

Cookie problems:

- Watch startup logs for `cookie account alive` or `cookie account bad`.
- Re-export cookies if the account is logged out, checkpointed, or blocked.
- If one account fails repeatedly, facebed cools it down and tries other configured accounts.

## Small benchmark vs the original Python project

The original project, [facebed/facebed](https://github.com/facebed/facebed), is a Python/Bottle
app. This repo is a Rust port.

One small crawl benchmark was run on June 16, 2026 from a residential IP in Vietnam, against one
cookie-viewable group post. Both apps ran on the same local machine and returned a valid embed with
the same title/image. The archived Python app's stale Cookie-Editor timestamp check was bypassed
locally so it could use the same cookie export.

| Implementation | Runs | Min | Median | Mean | Max |
| --- | ---: | ---: | ---: | ---: | ---: |
| Rust port | 4 | 1.14s | 1.43s | 1.63s | 2.55s |
| Original Python app | 4 | 3.77s | 4.61s | 5.01s | 7.03s |

Treat this as a sanity check, not a universal guarantee. Facebook response time depends heavily on
region, account health, link type, cookies, and whether a link needs share-resolution redirects.

Practical differences from the Python app:

| Area | Original Python app | Rust port |
| --- | --- | --- |
| Runtime | Python 3.12+ with Bottle, BeautifulSoup, requests, and helper packages. | Single Rust binary with async `axum` and `reqwest`. |
| Docker image | Python runtime plus site packages. | Static release binary copied into `scratch`. |
| Updates | Included a remote `/update` hook. | No remote self-update endpoint; redeploy normally. |
| Parser layout | One large `facebed.py`. | Parser modules by URL type. |
| Cookies | One `cookies.json`; stale timestamps disable cookies. | Multiple accounts, live startup checks, cooldowns, retries, and affinity. |
| URL coverage | Posts, photos, reels, watch, share links, and photo comments. | Keeps those and adds stories, Page-video routing, group multi-permalink cleanup, and more mobile-share handling. |

## Local development

Requires Rust 1.75 or newer.

Run with defaults:

```bash
cargo run
```

Run with config and cookies:

```bash
cargo run -- -c config.yaml --cookies cookies.json
```

Release build:

```bash
cargo build --release
./target/release/facebed -c config.yaml --cookies cookies.json
```

Verbose logs:

```bash
RUST_LOG=debug cargo run -- -c config.yaml --cookies cookies.json
```

Tests and formatting:

```bash
cargo test
cargo fmt -- --check
```

## Maintainer notes

- Keep the crawler user-agent gate. Humans should redirect to Facebook.
- Keep parser failures noisy. `P` errors with HTML attachments are how Facebook schema changes get
  fixed.
- Keep user-facing error code letters stable: `C`, `P`, `U`, `X`, and `T`.
