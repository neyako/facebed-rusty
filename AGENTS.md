# AGENTS.md

Guide for AI assistants working in this repo.

## What this is

`facebed` — Facebook URL → OpenGraph embed proxy for Discord/messaging apps. User replaces
`facebook.com` with `facebed.com`; server scrapes the post and returns minimal HTML with OG
meta tags. Crawlers (Discord, Slack, etc.) see the embed; humans get a 301 to Facebook.

Not affiliated with Meta. Single-maintainer project.

The codebase was ported from Python (Bottle) to **Rust** (axum + reqwest + scraper) in May 2026
for memory safety and a smaller deploy artifact. The original Python implementation has been
deleted — `git log` is the only place left to read it.

## Layout

```
Cargo.toml                cargo manifest
src/
  main.rs                 entry, clap args, axum server bootstrap
  config.rs               YAML config schema (host, port, timezone, banned_users, notifier_webhook)
  cookies.rs              CookieJar — round-robin multi-account, expiry warnings
  crawler.rs              UA regex — bots get embeds, humans get 301
  embed.rs                OpenGraph HTML output (full / reel / error / redirect)
  error.rs                FacebedError + error codes C (no data), P (parse), U (unknown), X (other)
  fetch.rs                Fetcher — HTTP, login-wall probe, JSON-blob extraction
  jq.rs                   recursive Value walker (first/all/has/last/enumerate)
  notifier.rs             Discord webhook — fire-and-forget tokio::spawn
  routes.rs               axum routes, URL dispatch (mirrors Python facebed.py:823 logic)
  url_clean.rs            strip mobile tracking params, share_url override
  parsers/
    mod.rs                ParsedPost struct, Parser trait, ParserCtx
    util.rs               Story (recursive attached_story), images_from_post, videos_from_post, video_link_in_node
    json_post.rs          default post (groups, /posts, /permalink.php, /story.php)
    single_photo.rs       /photo, /photo.php
    photocom.rs           image-in-comment (?type=3)
    reels.rs              /reel/<id>  ← BUG 1 FIX lives here (relaxed get_content_node)
    video_watch.rs        /watch
    stories.rs            24-hour stories /stories/<author_id>/<media_id>  ← BUG 3 NEW
assets/                   favicon, banner, index.html landing page
Dockerfile                multi-stage musl build → scratch image
docker-compose.yml        compose example with volume mounts
config.example.yaml       config template
cookies.example.json      cookies template (both single + multi-account shapes)
.github/workflows/
  docker-build.yml        multi-arch ghcr.io build (native runners, no QEMU)
```

No formatter config. No CI-enforced lint. Rust 1.75+ required (rust-version in Cargo.toml).

## Runtime config

YAML file passed via `-c <path>`. Defaults in `Config::default()` (config.rs). Keys:

- `host`, `port` — bind address.
- `timezone` — hours offset from UTC, -12..14, used by embed timestamps.
- `banned_users` — Vec of FB author IDs; matching posts return canned "Banned" embed.
- `notifier_webhook` — Discord webhook URL for parser failures + expired cookie alerts.

Validation in `Config::load()`. Missing keys fall through to defaults (serde `default` attr).

`cookies.json` — optional. Two formats accepted:
- Flat Cookie-Editor array → one account labeled `default`.
- `{"accounts": [{"label": ..., "entries": [...]}]}` → multi-account, round-robin per request.

Expired cookies trigger `Notifier::warn` to the Discord webhook on startup.

## Architecture

### Request flow

1. `axum::Router` (`routes.rs::router`) catches every path via `/*path` handler.
2. `?type=3` checked first → `PhotocomParser`.
3. Crawler UA gate (`crawler::is_crawler`) — non-crawler returns 301 + `format_redirect_page`.
4. `/share/{v,r,p}/...` → `fetch::resolve_share_link` (follow redirects via reqwest, then re-dispatch on resolved path).
5. `url_clean::clean_path` strips tracking params (`fs`, `mibextid`, `rdid`, `share_url`, etc).
6. `/videos/<id>` rewritten to `reel/<id>`.
7. Path matched by regex → one of the parsers:
   - `^/?stories/\d+/[A-Za-z0-9=_-]+` → `StoriesParser`
   - `^/?reel/[0-9]+` → `ReelsParser`
   - `^/*photo(\.php)*/*$` → `SinglePhotoParser`
   - `^/*watch` → `VideoWatchParser`
   - `is_facebook_url()` (groups/permalink/story/posts/photo) → `JsonPostParser`
   - else → error embed code `C`
8. Parser returns `ParsedPost`. `routes::render` picks `format_full_post_embed` (image card)
   or `format_reel_post_embed` (video card). If `video_links` non-empty, reel format wins.

### Parsers

All parsers implement `async_trait::Parser::process(ctx, post_path) -> FacebedResult<ParsedPost>`.
They call `ctx.fetcher.fetch(post_path, use_cookies)` to get a `FetchedPage`, then use
`get_json_blocks` + `jq::first/all/has` to drill into FB's JSON blobs.

- **`JsonPostParser`** (`parsers/json_post.rs`) — default post. Tries `data.comet_ufi_summary_and_actions_renderer`, then `node_v2.comet_sections`, then `node.comet_sections`, then group hoisted feed. Builds a `Story` (`util::Story::from_json`) which recursively handles `attached_story` for shared posts.
- **`SinglePhotoParser`** (`single_photo.rs`) — `/photo` URLs. Uses `prefetch_uris_v2` for the image.
- **`PhotocomParser`** (`photocom.rs`) — `?type=3` image-in-comment. Adds `(💬)` suffix to author.
- **`ReelsParser`** (`reels.rs`) — short-form video. **Bug 1 fix:** `find_content_node` matches `creation_story` with `short_form_video_context` OR `videoDeliveryResponseFragment` (modern field) OR `videoDeliveryLegacyFields` OR `playable_url`. Owner-with-name found by scanning every block (the rich owner lives in a different block from `creation_story`).
- **`VideoWatchParser`** (`video_watch.rs`) — `/watch` URLs. Generic-watch-feed canonical link → `NoData`.
- **`StoriesParser`** (`stories.rs`) — NEW. Searches blocks for `unified_stories_with_notes.edges[0].node`, pulls `playable_url` (video) or `image.uri` (photo) from `attachments[0].media`. Owner from `bucket.owner.name`. Expired or login-walled story → `NoData` (24h auto-expiry).

### Video URL extraction (parsers/util.rs::video_link_in_node)

Multi-strategy probe, in order:
1. **Modern:** `videoDeliveryResponseFragment.videoDeliveryResponseResult.progressive_urls[].progressive_url`
2. **Legacy:** `videoDeliveryLegacyFields.browser_native_hd_url` / `browser_native_sd_url` (now usually `null` — fallback only)
3. **Direct:** `playable_url_quality_hd` then `playable_url` (used by Stories, sometimes Reels)

Adding a new FB schema variant? Add a probe to this function — every parser uses it.

### Jq helpers (`jq.rs`)

Recursive walker over `serde_json::Value`. `jq::first(root, "key")` finds first occurrence
anywhere in the tree. `jq::all` collects all. `jq::has(root, &["k1", "k2"])` AND-existence.
This is how the code stays robust against FB shuffling their JSON shape — never hardcode a path,
search by key.

### Error codes (error.rs)

- `NoData` → code **C** — login wall / restricted content / expired story. No webhook alert.
- `Parse { html, url }` → code **P** — parser bug. `fetch.rs` attaches raw HTML if missing.
  `routes::error_response` posts the HTML file to Discord webhook for offline triage.
- Http/Io/Json/Yaml → code **U**.
- Other anyhow → code **X**.

Codes appear as `[X]` suffix in the error embed title. User-visible. Don't rename.

### Login-wall detection (fetch.rs::probe_page_type)

Checked inside `Fetcher::fetch` before parser sees the page. Triggers:
1. `<link rel=canonical>` href matches `/login\b`
2. `<meta http-equiv=refresh>` content has `URL=/login`
3. JSON blocks contain `login_data` or `useCometLogInFormQuery`
4. Absence of `i18n_reaction_count` (no post data)

### Multi-cookie / multi-account (cookies.rs)

`CookieJar` holds a `Vec<CookieAccount>`. `next_account()` round-robins per request (atomic
counter). Each account has a label + entries. Header-based attachment (no reqwest cookie store).
Expired cookies warned once at startup.

## Patterns to follow

- **New URL scheme**: add parser file under `src/parsers/`, declare in `parsers/mod.rs`,
  add `ParserKind` variant + regex in `routes.rs`, wire dispatch order BEFORE `is_facebook_url`
  fallback.
- **Robust JSON extraction**: never index FB JSON by hardcoded path. Use `jq::first/all/has` to
  search by key. If you must hardcode a path because key names collide, add a TODO + recon notes.
- **HTML output**: `format!` with raw strings `r##"..."##` (need double-pound when content has
  `"#` like CSS color codes). Always `html_escape::encode_quoted_attribute` (via `escape_attr` in
  embed.rs) for user-derived text and `quote()` for URLs.
- **Errors**: build via `FacebedError::parse_with(msg, html, url)` so the html gets attached to
  Discord. Use short tags like `(cn)`/`(vn)`/`(own)` matching where it failed — useful when
  grepping logs.
- **Warnings to maintainer**: `notifier.warn(msg, Some((filename, bytes)))` — non-blocking,
  spawned. No-ops without webhook configured.

## What NOT to do

- Don't add a heavy web framework (rocket, actix). Stick with axum.
- Don't refactor parsers into a single base struct — each FB URL type has a genuinely different
  JSON shape, the per-file duplication is intentional.
- Don't change error code letters `C/P/U/X` — user-visible in embed titles, used to triage from
  screenshots.
- Don't drop the crawler-UA check — humans must get redirected, not the embed.

## Running

Local: `cargo run -- -c config.yaml` (or no `-c` for built-in defaults).
Docker: `docker compose up -d --build`.

To verify changes: hit `localhost:9812/<facebook-path>` with `curl -A 'Discordbot/2.0'` and
inspect the returned HTML's OG tags.

`RUST_LOG=debug` for verbose tracing.

## Tests

`cargo test` — currently covers `jq`, `crawler`, `url_clean`. Parser tests need golden HTML
fixtures (TODO). When adding/fixing a parser, capture failing HTML into `/tmp/recon_out/` via
the recon script (see commit history) and add a unit test if practical.

## Commit style

Look at `git log --oneline` — short imperative subjects, no Conventional Commits prefix,
body only when "why" isn't obvious. Match that.
