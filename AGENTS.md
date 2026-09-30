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
  main.rs                 entry, clap args, startup cookie check, SIGHUP reload, axum bootstrap
  config.rs               YAML config schema (host, port, timezone, banned_users, notifier_webhook)
  cookies.rs              CookieJar: accounts from cookies*.json + useragents.json, cooldown,
                          failure counts, per-group/profile affinity
  crawler.rs              UA regex: bots get embeds, humans get 301
  embed.rs                OpenGraph HTML output (full / reel / oversized / error / timeout / redirect)
  embed_cache.rs          90s cache of rendered embeds, Activity posts, resolved share links
  ttl_map.rs              TtlMap: the bounded expiring map behind every cache
  markdown.rs             group-post Markdown tokenizer shared by embed.rs and activity.rs
  activity.rs             Mastodon-style Activity JSON Discord uses for share links
  activity_test.rs        its tests
  error.rs                FacebedError + user-visible codes C / P / U / X (and T for timeouts)
  fetch.rs                Fetcher: HTTP, login-wall check, share resolution, JSON-blob extraction;
                          fetch_until caps streaming at PARTIAL_STREAM_DEADLINE (4.5s)
                          and flags cut prefixes via FetchedPage::was_cut_by_deadline
  jq.rs                   recursive Value walker (first/all/has/enumerate)
  notifier.rs             Discord webhook, fire-and-forget tokio::spawn
  routes/
    mod.rs                router, catch-all embed handler, scrape-and-render, error responses
    dispatch.rs           path rewrites + parser choice (`route`), ParserKind, affinity scope key
    activity.rs           Activity endpoints + share-discovery scrape
    race.rs               account race (`race_identities`), account health
  url_clean.rs            strip mobile tracking params, share_url override
  parsers/
    mod.rs                ParsedPost struct, Parser trait, ParserCtx
    util.rs               Story (recursive attached_story), images_from_post, video_link_in_node
    json_post.rs          default post (groups, /posts, /permalink.php, /story.php)
    single_photo.rs       /photo, /photo.php
    photocom.rs           image-in-comment (?type=3), album-photo fallback
    reels.rs              /reel/<id>
    video_watch.rs        /watch and <page>/videos/<id>
    stories.rs            24-hour stories /stories/<author_id>/<media_id>
    comment.rs            comment permalink (?comment_id=)
    fixtures.rs           parser tests on minimized real pages + the capture tool
    fixtures/             the fixture pages
assets/                   favicon, banner, index.html (compiled into the binary), README images
Dockerfile                multi-stage musl build → scratch image
docker-compose.yml        compose example with volume mounts
config.example.yaml       config template
cookies.example.json      cookies template (both single + multi-account shapes)
useragents.example.json   per-account user-agent sidecar template
.github/workflows/
  docker-build.yml        fmt + clippy + test, then multi-arch ghcr.io build (native runners)
```

Formatting: `cargo fmt` (rustfmt, 2021 edition). CI enforces `cargo fmt --check`,
`cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked`
(`.github/workflows/docker-build.yml`), so run all three before pushing.
Rust 1.86+ required (rust-version in Cargo.toml).

## Runtime config

YAML file passed via `-c <path>`. Defaults in `Config::default()` (config.rs). Keys:

- `host`, `port`: bind address.
- `timezone`: hours offset from UTC, -12..14, used by embed timestamps.
- `banned_users`: Vec of FB author IDs; matching posts return a canned "Banned" embed.
- `notifier_webhook`: Discord webhook URL for parser bugs and cookie-account alerts.

Validation in `Config::load()`. Missing keys fall through to defaults (serde `default` attr).

CLI flags: `-c/--config`, `--cookies <path>` (default `./cookies.json`),
`--dump <fb path>` + `--dump-dir` (fetch one page with the cookie, write `page.html`
and every JSON block, exit; the input for parser fixtures).

Cookies are optional. `cookies.json` plus every sibling `cookies*.json` is loaded,
each as either:
- a flat Cookie-Editor array: one account, labeled from the file name
  (`cookies-alice.json` → `alice`, `cookies.json` → `default`), also accepted
  wrapped as `{"url": ..., "cookies": [...]}`, or
- `{"accounts": [{"label": ..., "entries": [...]}]}`: several accounts.

`useragents.json` next to them maps labels to a user agent. At startup every account
is checked live against Facebook; bad ones page the webhook. `SIGHUP` reloads config
and cookies (`CookieJar::load_strict`: a malformed file or unreadable directory keeps the running jar;
`inherit_state` keeps affinity, and cooldowns for accounts whose cookie is unchanged).

## Architecture

### Request flow

1. `axum::Router` (`routes/mod.rs::router`) serves `/`, `/favicon.ico`, `/banner.png`,
   `/healthz` (counts only), `/oembed.json` (author/provider line Discord reads),
   the Activity routes `/api/v1/statuses/:id` and `/users/:user/statuses/:id`, and
   every other path via `catch_all`.
2. Crawler UA gate (`crawler::is_crawler`): non-crawlers get a 301 + `format_redirect_page`.
3. Share links (`share/...`): Discordbot requests with a usable origin get the Activity
   flow (`share_activity_response`); others resolve via `fetch::resolve_share_link`
   and re-dispatch on the resolved path. Resolutions are cached 90s.
4. `url_clean::clean_path` strips tracking params (`fs`, `mibextid`, `rdid`, `share_url`, etc).
5. Rewrites: `groups/<id>/?multi_permalinks=<post>` → group post,
   `<page>/photos/<slug>/<id>` → `photo.php?fbid=<id>`, `/videos/<id>` → `reel/<id>`,
   `reel/<a>/<b>` → `reel/<b>` (query strings preserved).
6. Dispatch, in order:
   - `type=3` query → `PhotocomParser`
   - `comment_id` query → `CommentParser`; on NoData `(ccn)` the comment params are
     stripped and the path re-dispatched (post embed instead of error C)
   - `^/?stories/\d+/[A-Za-z0-9=_-]+` → `StoriesParser`
   - `^/?reel/[0-9]+` → `ReelsParser`
   - `^/*photo(\.php)*/*$` → `SinglePhotoParser`
   - `^/*watch` or `<page>/videos/<id>` → `VideoWatchParser`
   - `is_facebook_url()` (groups/permalink/story/posts/photo) → `JsonPostParser`
   - else → error embed code `C`
7. `process`: rendered embeds are cached 90s; at most `MAX_INFLIGHT_FETCHES` (16)
   scrapes run at once, the rest get a fast 503. The scrape races accounts
   (see Multi-cookie below) under an 8.5s budget (`DISCORD_RESPONSE_BUDGET`, Discord
   gives up at ~10s); past it the timeout embed `[T]` is returned.
8. `routes::render` picks `format_full_post_embed` (image card) or
   `format_reel_post_embed` (video card). Reels/Watch and video-only posts use the
   video card; mixed posts use the image grid.

### Parsers

Each parser implements `Parser::process(ctx, post_path) -> FacebedResult<ParsedPost>`
(native async fn in trait, called statically from `run_parser`). `process` fetches
with `ctx.fetcher.fetch(post_path)` and hands the `FetchedPage` to a sync parse
function (`parse_page`, `parse_reel`, `parse_fetched_post`) that tests call directly.
Parsing uses `get_json_blocks` + `jq::first/all/has` to drill into FB's JSON blobs.

- **`JsonPostParser`** (`parsers/json_post.rs`) — default post. Tries `data.comet_ufi_summary_and_actions_renderer`, then `node_v2.comet_sections`, then `node.comet_sections`, then group hoisted feed. Builds a `Story` (`util::Story::from_json`) which recursively handles `attached_story` for shared posts.
- **`SinglePhotoParser`** (`single_photo.rs`) — `/photo` URLs. Uses `prefetch_uris_v2` for the image.
- **`PhotocomParser`** (`photocom.rs`) — `?type=3` image-in-comment. Adds `(💬)` suffix to author. FB also puts `type=3` on plain album photos (`set=a.<album>`); no attached comment → reuses the fetched page via `single_photo::parse_page`.
- **`ReelsParser`** (`reels.rs`) — short-form video. `find_content_node` matches `creation_story` with `short_form_video_context` OR `videoDeliveryResponseFragment` (modern field) OR `videoDeliveryLegacyFields` OR `playable_url`. Owner-with-name found by scanning every block (the rich owner lives in a different block from `creation_story`). Reads the full page every time — the early-stop scanner was removed (2026-08-29): FB moved the marker blocks late (79–92% of body) and any mispredicted stop forced a second full fetch (~1.8s wasted), more than the early stop ever saved.
- **`VideoWatchParser`** (`video_watch.rs`) — `/watch` and `<page>/videos/<id>` URLs. Generic-watch-feed canonical link → `NoData`.
- **`StoriesParser`** (`stories.rs`) — Searches blocks for `unified_stories_with_notes.edges[0].node`, pulls `playable_url` (video) or `image.uri` (photo) from `attachments[0].media`. Owner from `bucket.owner.name`. Expired or login-walled story → `NoData` (24h auto-expiry).
- **`CommentParser`** (`comment.rs`) — `?comment_id=` permalinks. Finds the comment node by `legacy_fbid`, `comment_id=` inside url fields, or base64-decoding node `id` (`comment:<post>_<id>`). Author rendered as `Name (💬)`; media via the shared `images_from_post`/`video_link_in_node` probes. Comment not server-rendered → `NoData` `(ccn)` and routes re-dispatches the stripped path.

### Video URL extraction (parsers/util.rs::video_link_in_node)

Multi-strategy probe, in order:
1. **Modern:** `videoDeliveryResponseFragment.videoDeliveryResponseResult.progressive_urls[].progressive_url`
2. **Legacy:** `videoDeliveryLegacyFields.browser_native_hd_url` / `browser_native_sd_url` (now usually `null` — fallback only)
3. **Direct:** `playable_url_quality_hd` then `playable_url` (used by Stories, sometimes Reels)

Adding a new FB schema variant? Add a probe to this function — every parser uses it.

### Video size probe (routes/mod.rs::render_with_size_check)

`head_content_length` HEADs the video URL (~0.6s) to catch >25 MB files Discord's
media proxy refuses to inline. Skipped once the request is older than
`VIDEO_PROBE_SKIP_AFTER` (6s) — past that the crawler budget matters more than
the oversize fallback; skipping behaves like a missing Content-Length header
(render inline, let Discord try).

### Jq helpers (`jq.rs`)

Recursive walker over `serde_json::Value`. `jq::first(root, "key")` finds first occurrence
anywhere in the tree. `jq::all` collects all. `jq::has(root, &["k1", "k2"])` AND-existence.
This is how the code stays robust against FB shuffling their JSON shape — never hardcode a path,
search by key.

### Error codes (error.rs)

- `NoData` → code **C** — restricted content / private group / expired story. No webhook alert.
- `LoginWall` → code **C** — FB ignored the cookie. Counts against the account (see below).
- `RateLimited`, `Checkpointed` → code **C**, and cool the account down.
- `Parse { html, url }` → code **P** — parser bug. Build it with `parse_with` so the page
  HTML rides along; `routes::error_response` posts it to the webhook for offline triage.
- Http/Json → code **U**.
- Other anyhow → code **X**.
- The timeout embed (`format_timeout_embed`) shows **T**; it is not a `FacebedError`.

Codes appear as `[X]` suffix in the error embed title. User-visible. Don't rename.

### Login-wall detection (fetch.rs::is_login_wall)

Checked in `FetchedPage::from_html` before a parser sees the page. A login wall is:
1. `<link rel=canonical>` href matching `/login\b`, or
2. `<meta http-equiv=refresh>` content with `URL=/login`, or
3. `login_data` / `useCometLogInFormQuery` in the body **and** no `i18n_reaction_count`
   (post data wins over a stray login preloader).

### Multi-cookie / multi-account (cookies.rs, routes/race.rs)

`CookieJar` holds a `Vec<CookieAccount>`, each with a label + entries. Header-based
attachment (no reqwest cookie store).

`scrape_with_accounts` → `race_identities` races identities from `account_order`
(healthy in configured priority, the last winner for this group/profile hoisted first,
cooled-down last). The next identity starts on failure **or** after `ACCOUNT_HEDGE_AFTER`
(3s) with no result; first success wins and the rest are aborted. FB randomly
slow-drips page bodies (~0.3 MB/s vs ~2 MB/s), so the hedge matters even with one
account: then it re-runs the same account, and drops that re-read if it isn't done
within 3s (a slow re-read can never overtake the original on the same page; waiting
for it only delayed private-group errors to ~7.7s). Only healthy accounts hedge;
nothing starts with less than `ATTEMPT_MIN_REMAINING` (3s) of budget left. `fetch::ACCOUNT_OVERRIDE`
carries the attempt's identity (`Some(index)` or `None` = guest).

Guest (no cookies) is the last resort, started only after a failure, never as the
slow-read hedge. From the US VPS (tested 2026-09-29) guests load public page/profile
posts and `share/p`/`share/v` targets, but hit a login wall on reels, `photo.php` and
group posts. It rescues posts whose author blocked our account or a dead cookie. When
everything fails, `final_error` reports a cookie account's `Parse` error first so
parser bugs still reach the webhook.

Accounts are penalized (cooldown + failure count toward the @everyone alert) only on
account-level errors: `Checkpointed`, `RateLimited`, `LoginWall`. Losing a race is not
evidence: private groups fail everywhere, slow-drips are transient, and "not a member
of this group" is per-scope, which affinity already handles. Affinity keys are
`groups/<id>` / `user/<name>` only; reels/watch/photo.php use priority order.

## Patterns to follow

- **New URL scheme**: add parser file under `src/parsers/`, declare in `parsers/mod.rs`,
  add `ParserKind` variant + regex in `routes/dispatch.rs`, wire it into `select_kind` BEFORE the `is_facebook_url`
  fallback. Keep fetch and parse separate so a fixture test can cover it.
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
- Don't change error code letters `C/P/U/X/T` — user-visible in embed titles, used to triage from
  screenshots.
- Don't drop the crawler-UA check — humans must get redirected, not the embed.

## Running

Local: `cargo run -- -c config.yaml` (or no `-c` for built-in defaults).
Docker: `docker compose up -d --build`. Assets are compiled in; the image is just the binary.

To verify changes: hit `localhost:9812/<facebook-path>` with `curl -A 'Discordbot/2.0'` and
inspect the returned HTML's OG tags.

`RUST_LOG=debug` for verbose tracing.

## Tests

`cargo test --locked`: focused unit tests next to the code, including the
account race on a paused clock (`routes/race.rs`) and parser fixtures (`parsers/fixtures.rs`).

Parser fixtures are real pages minimized to the JSON the output depends on. When
Facebook changes a shape, capture the page with `--dump`, run the ignored
`capture_fixture` tool (usage in `parsers/fixtures.rs`), and assert the fields that
matter. The repo is public: only use public Page content, and read the fixture before
committing it (the tool strips tokens and redacts the account, but check).

## Commit style

Look at `git log --oneline` — short imperative subjects, no Conventional Commits prefix,
body only when "why" isn't obvious. Match that.
