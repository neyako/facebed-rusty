# Plan 014 (SPIKE): Stream Facebook CDN media through a `/media` passthrough route

> **Executor instructions**: This is a **spike** — the goal is a working,
> host-guarded `/media` route plus a written recommendation, NOT rewiring the
> embed output. Follow the steps, run every verification command, and at the end
> fill in the "Spike outcome" section of this file with what you measured/found.
> If anything in "STOP conditions" occurs, stop and report. When done, update the
> status row in `plans/README.md` — unless a reviewer dispatched you and told you
> they maintain the index.
>
> **Drift check (run first)**: `git diff --stat 2e6610f..HEAD -- src/routes.rs Cargo.toml`
> If either changed since this plan was written, compare the "Current state"
> excerpts against the live code before proceeding; on a mismatch, treat it as a
> STOP condition.

## Status

- **Priority**: P3
- **Effort**: M-L
- **Risk**: MED
- **Depends on**: none (but security-sensitive — see "Why" and STOP conditions)
- **Category**: direction
- **Planned at**: commit `2e6610f`, 2026-06-18

## Why this matters

Today the embed HTML points `og:image` (`src/embed.rs:252`) and `og:video`
(`src/embed.rs:311`) directly at raw `fbcdn.net` / `scontent.*.fbcdn.net` URLs.
Those URLs are **signed and time-limited**. Discord caches images at embed time
so stills usually survive, but `og:video` is fetched live on playback — an
expired signed URL means the video is **dead** when someone opens the post a day
later. There is no way to make the media durable today because nothing serves the
bytes from facebed's own domain.

This spike builds a `GET /media?u=<fbcdn-url>` route that streams the media
through facebed (so the client only ever sees a stable `facebed.example.com/media?...`
URL), guarded by the **same Facebook-host allowlist** the security work in plan
004 established (`is_facebook_media_host`). The spike answers one question for the
maintainer: *is routing media through facebed worth the bandwidth/abuse cost to
fix video-embed expiry?* It deliberately does **not** rewrite the embed output —
that decision is the spike's deliverable, written up at the end.

**Security framing (read before coding)**: `u` is an attacker-controllable URL. If
you fetch it without validating the host, this route is a classic SSRF /
open-proxy. The host allowlist guard in Step 3 is **not optional** — it is the
whole reason this is safe. This is exactly the boundary plan 004 added to the
fetch path; you are extending it to a new sink.

## Current state

- `src/routes.rs` — router + response helpers (you'll add a route + handler here):
  ```rust
  // src/routes.rs:38-46
  pub fn router(state: AppState) -> Router {
      Router::new()
          .route("/", get(root))
          .route("/oembed.json", get(oembed))
          .route("/favicon.ico", get(favicon))
          .route("/banner.png", get(banner))
          .route("/*path", get(catch_all))
          .with_state(state)
  }
  ```
  - `AppState.fetcher: Arc<Fetcher>` is available (`src/routes.rs:34`).
  - `Fetcher::client()` returns the shared `reqwest::Client`:
    ```rust
    // src/fetch.rs:144
    pub fn client(&self) -> &Client { ... }
    ```
  - Query extraction pattern already in use (`OEmbedParams`,
    `src/routes.rs:98-109`): a `#[derive(serde::Deserialize)]` struct + `Query`.
- `src/url_clean.rs` — the host allowlist (already public, used by
  `head_content_length` at `src/fetch.rs:260`):
  ```rust
  // src/url_clean.rs:86-90
  pub fn is_facebook_media_host(host: &str) -> bool {
      let host = host.to_ascii_lowercase();
      is_facebook_page_host(&host) || host == "fbcdn.net" || host.ends_with(".fbcdn.net")
  }
  ```
- `Cargo.toml` — `reqwest` (`rustls-tls`, `gzip`, `brotli`, `http2`, `stream`?
  check features), `axum` 0.7 with `http1`/`tokio`/`query`, `url` 2, `tokio`.
  **`reqwest`'s `stream` feature may not be enabled** — `bytes_stream()` requires
  it. Verify in Step 1.
- **Convention**: response helpers build a `HeaderMap` then
  `(StatusCode, headers, body).into_response()` — see `json_response`
  (`src/routes.rs:89`). Match it.

## Commands you will need

| Purpose   | Command                          | Expected on success       |
|-----------|----------------------------------|---------------------------|
| Build     | `cargo build`                    | exit 0                    |
| Tests     | `cargo test`                     | all pass (88 today + new) |
| One test  | `cargo test media_guard`         | the new test(s) pass      |
| Format    | `cargo fmt -- --check`           | exit 0, no diff           |
| Feature   | `cargo tree -i reqwest -e features` | shows whether `stream` is on |

## Scope

**In scope** (the only files you should modify):
- `src/routes.rs` — `/media` route, handler, host-guard helper, unit tests.
- `Cargo.toml` — **only** if `reqwest`'s `stream` feature must be added (one
  feature flag, no new crate). If it's already on, do not touch `Cargo.toml`.
- `plans/014-media-proxy-spike.md` — fill in "Spike outcome" at the end.
- `plans/README.md` — status row only.

**Out of scope** (do NOT touch):
- `src/embed.rs` — **do not rewrite `og:image`/`og:video` to point at `/media`
  in this spike.** Whether to do that is the spike's *recommendation*, decided
  after measuring. Rewiring it now would couple an unproven route into every
  embed.
- `src/fetch.rs` — reuse `client()`; add no methods there.
- Any new crate dependency. Caching of proxied media, Range/partial-content
  support, and on-disk media cache are all **deferred** (note them in the outcome).

## Git workflow

- Branch: `advisor/014-media-proxy`
- Commit style: short imperative subject, no Conventional Commits prefix (e.g.
  "Add /media passthrough spike for Facebook CDN").
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Confirm `reqwest` streaming is available

Run `cargo tree -i reqwest -e features`. If the feature list does **not** include
`stream`, add it in `Cargo.toml` to the `reqwest` features array (append
`"stream"` to the existing list — do not remove any existing feature).

**Verify**: `cargo build` → exit 0 after the edit (or no edit needed).

### Step 2: Add the query struct and route

In `src/routes.rs`:

```rust
#[derive(serde::Deserialize)]
struct MediaParams {
    #[serde(default)]
    u: String,
}
```

Register the route in `router` before the catch-all:

```rust
.route("/media", get(media))
```

### Step 3: Implement the host-guarded passthrough handler

The guard is the security core. Validate the URL parses, is absolute (`http`/
`https`), and its host passes `is_facebook_media_host` — **before any fetch**.

```rust
async fn media(
    State(state): State<AppState>,
    axum::extract::Query(p): axum::extract::Query<MediaParams>,
) -> Response {
    // 1. Validate target host — refuse anything not on the FB media allowlist.
    let Ok(parsed) = Url::parse(&p.u) else {
        return (StatusCode::BAD_REQUEST, "bad url").into_response();
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return (StatusCode::BAD_REQUEST, "bad scheme").into_response();
    }
    let host_ok = parsed
        .host_str()
        .map(crate::url_clean::is_facebook_media_host)
        .unwrap_or(false);
    if !host_ok {
        return (StatusCode::FORBIDDEN, "host not allowed").into_response();
    }

    // 2. Stream the upstream response through.
    let upstream = match state.fetcher.client().get(parsed).send().await {
        Ok(r) => r,
        Err(_) => return (StatusCode::BAD_GATEWAY, "upstream error").into_response(),
    };
    if !upstream.status().is_success() {
        return (StatusCode::BAD_GATEWAY, "upstream status").into_response();
    }

    // 3. Cap by Content-Length when present (defense against huge transfers).
    const MAX_MEDIA_BYTES: u64 = 30 * 1024 * 1024; // ~30 MB
    if let Some(len) = upstream.content_length() {
        if len > MAX_MEDIA_BYTES {
            return (StatusCode::PAYLOAD_TOO_LARGE, "too large").into_response();
        }
    }

    // 4. Pass through content-type; stream the body.
    let ct = upstream
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("application/octet-stream"));
    let mut headers = HeaderMap::new();
    headers.insert(axum::http::header::CONTENT_TYPE, ct);
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    let body = axum::body::Body::from_stream(upstream.bytes_stream());
    (StatusCode::OK, headers, body).into_response()
}
```

Notes:
- `Url` is imported (`use url::Url;`, `src/routes.rs:27`). `State`, `get`,
  `HeaderMap`, `HeaderValue`, `StatusCode`, `Response` all imported already.
- The `Content-Length` cap only catches upstreams that *declare* a length. A
  chunked upstream with no length is not capped here — note that limitation in the
  outcome (a streaming byte-counter is the deferred follow-up).

**Verify**: `cargo build` → exit 0; `cargo fmt -- --check` → exit 0.

### Step 4: Unit-test the host guard (the security-critical part)

Factor the guard decision into a pure helper so it's testable without a network:

```rust
/// Returns true iff `u` is a fetchable Facebook media URL.
fn media_target_allowed(u: &str) -> bool {
    match Url::parse(u) {
        Ok(parsed) => matches!(parsed.scheme(), "http" | "https")
            && parsed.host_str().map(crate::url_clean::is_facebook_media_host).unwrap_or(false),
        Err(_) => false,
    }
}
```

Call `media_target_allowed(&p.u)` from the handler instead of inlining the checks.
Then test:

```rust
#[test]
fn media_guard_blocks_non_facebook_hosts() {
    assert!(media_target_allowed("https://scontent.xx.fbcdn.net/v/x.jpg"));
    assert!(media_target_allowed("https://video.fbcdn.net/v.mp4"));
    assert!(!media_target_allowed("https://evil.example.com/x.jpg"));
    assert!(!media_target_allowed("https://evilfbcdn.net/x.jpg")); // lookalike
    assert!(!media_target_allowed("file:///etc/passwd"));
    assert!(!media_target_allowed("http://169.254.169.254/latest/meta-data")); // SSRF target
    assert!(!media_target_allowed("not a url"));
}
```

**Verify**: `cargo test media_guard` → passes. `cargo test` → all pass.

### Step 5: Manual durability check + write the outcome

(Manual, for the spike finding — not a `cargo` gate.) Start the server, take a
real `og:video` URL from a current reel embed (hit a reel path with
`curl -A 'Discordbot/2.0'` and read the `og:video` content), then:
- `curl -sI "localhost:9812/media?u=<that-url>"` → `200`, correct `Content-Type`.
- `curl -sI "localhost:9812/media?u=https://example.com/x"` → `403`.

Then **fill in the "Spike outcome" section below** with: did the proxy serve the
media; approximate size/latency of a typical video; and your recommendation on
whether `og:video` (and/or `og:image`) should be rewritten through `/media` in a
follow-up — weighing durable embeds against the bandwidth cost of facebed now
serving video bytes for every playback.

## Test plan

- New tests `media_guard_blocks_non_facebook_hosts` (and any host edge cases) in
  the `src/routes.rs` tests module. Model after the existing pure-function tests
  there. **The SSRF/lookalike cases are the load-bearing ones** — they prove the
  open-proxy can't be turned against internal hosts.
- Manual durability + 403 checks from Step 5.

## Done criteria

ALL must hold:

- [ ] `cargo build` exits 0
- [ ] `cargo fmt -- --check` exits 0 (no diff)
- [ ] `cargo test` exits 0; `media_guard_blocks_non_facebook_hosts` passes,
      including the `169.254.169.254` and `evilfbcdn.net` cases
- [ ] `grep -n '"/media"' src/routes.rs` shows the route registered
- [ ] `grep -n "media_target_allowed" src/routes.rs` shows the guard used by both
      the handler and the test
- [ ] `src/embed.rs` is **unchanged** (`git diff --stat -- src/embed.rs` empty)
- [ ] If `Cargo.toml` was touched, the diff adds only the `stream` feature to
      `reqwest` and nothing else
- [ ] The "Spike outcome" section below is filled in
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report (do not improvise) if:

- The drift check shows `src/routes.rs`/`Cargo.toml` changed and excerpts no
  longer match.
- Making streaming work would require a **new crate** (not just a `reqwest`
  feature flag) — report it; do not add the dependency.
- `is_facebook_media_host` no longer exists or changed signature — the guard
  depends on it; do not reimplement the allowlist inline differently.
- `Fetcher::client()` is gone or no longer returns a `reqwest::Client`.
- You find yourself needing to edit `src/embed.rs` to make the spike testable —
  that's out of scope; the route must stand alone.

## Maintenance notes

For whoever owns this next:

- **This is a spike, not the finished feature.** Deferred and noted on purpose:
  Range/partial-content (`206`) support for video seeking, an actual byte-cap on
  chunked/unknown-length upstreams, and any caching of proxied media. A naive
  proxy re-fetches FB on every playback — fine to measure, not necessarily fine to
  ship for a busy instance.
- **Security**: the host guard is the only thing standing between this route and
  an open proxy / SSRF. Any change to it must keep the allowlist + scheme check.
  A reviewer should treat `media_target_allowed` as security-critical code and
  scrutinize its tests.
- **Bandwidth/abuse**: once embeds point through `/media`, facebed serves the
  media bytes for every crawler and every human playback — that's real egress and
  a new abuse vector. Pair any rollout with plan 011's in-flight cap (and consider
  a separate, tighter cap for `/media`).

## Spike outcome

> _Executor: fill this in after Step 5 before marking the plan DONE._

- Did `/media` successfully proxy a live `og:video` URL? (yes/no, status, CT)
- Typical video size / first-byte latency observed:
- Does the `Content-Length` cap fire on oversized media, and do chunked upstreams
  slip past it?
- **Recommendation**: should a follow-up rewrite `og:video` (and/or `og:image`)
  through `/media`? Trade-off in 2-3 sentences (durable embeds vs. egress cost).
- Any follow-up plan worth filing (caching, Range support, per-`/media` rate cap)?
