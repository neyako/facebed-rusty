# Plan 013: Add a `/healthz` endpoint exposing liveness, counters, and cookie state

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: `git diff --stat 2e6610f..HEAD -- src/routes.rs src/main.rs`
> If either changed since this plan was written, compare the "Current state"
> excerpts against the live code before proceeding; on a mismatch, treat it as a
> STOP condition.

## Status

- **Priority**: P3
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none
- **Category**: direction
- **Planned at**: commit `2e6610f`, 2026-06-18

## Why this matters

`facebed` is self-hosted and operational visibility is the main gap. Cookie
health, per-account cooldown, and request volume are **logged** but not
**queryable** — to know whether an account is in cooldown right now you have to
grep stdout. There is no health endpoint at all: the only routes are `/`,
`/oembed.json`, `/favicon.ico`, `/banner.png`, and the catch-all
(`src/routes.rs:38-46`). A reverse proxy / uptime monitor / container
orchestrator has nothing cheap to poll, and the operator can't see at a glance
"3 of 4 cookie accounts are alive, 1 is cooling down."

This plan adds a `GET /healthz` endpoint returning JSON: process uptime, a few
request counters, and a per-account snapshot (label + whether it's in cooldown).
It reuses the cookie introspection methods that already exist (`len`, `label_at`,
`in_cooldown`) and adds a tiny in-process counters struct. It exposes **no cookie
values** — only operator-chosen labels and booleans.

## Current state

- `src/routes.rs` — router and response helpers:
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
  ```rust
  // src/routes.rs:89-96 — a JSON response helper already exists; reuse it
  fn json_response(body: String) -> Response {
      let mut headers = HeaderMap::new();
      headers.insert(
          axum::http::header::CONTENT_TYPE,
          HeaderValue::from_static("application/json; charset=utf-8"),
      );
      (StatusCode::OK, headers, body).into_response()
  }
  ```
  ```rust
  // src/routes.rs:29-36 — AppState
  #[derive(Clone)]
  pub struct AppState {
      pub config: Arc<Config>,
      pub ctx: Arc<ParserCtx>,
      pub notifier: Notifier,
      pub fetcher: Arc<Fetcher>,
      pub embed_cache: Arc<std::sync::Mutex<crate::embed_cache::EmbedCache>>,
  }
  ```
  - `process` (`src/routes.rs:405`) is the function every dynamic Facebook render
    flows through — the natural place to bump a request counter.
- `src/parsers/mod.rs` — `ParserCtx` holds `cookies: Arc<arc_swap::ArcSwap<CookieJar>>`
  (`src/parsers/mod.rs:34`). `AppState.ctx.cookies.load()` gives a `&CookieJar`.
- `src/cookies.rs` — the introspection methods this plan reads (already public,
  do NOT modify them):
  ```rust
  pub fn len(&self) -> usize                       // src/cookies.rs:304
  pub fn in_cooldown(&self, i: usize) -> bool      // src/cookies.rs:385
  pub fn label_at(&self, i: usize) -> Option<&str> // src/cookies.rs:401
  ```
- **Convention**: shared state is `Arc<...>` on `AppState`, built in `main.rs`
  (the `embed_cache` field from plan 008 is the exemplar). JSON is built with
  `serde_json::json!{...}.to_string()` and returned via `json_response` — see
  `build_oembed_json` at `src/routes.rs:114-129` for the exact idiom.
- `serde_json` is a dependency; `std::sync::atomic` and `std::time::Instant` are
  std (`Instant` already imported at `src/routes.rs:25` via `std::time::{Duration, Instant}`).

## Commands you will need

| Purpose   | Command                       | Expected on success       |
|-----------|-------------------------------|---------------------------|
| Build     | `cargo build`                 | exit 0                    |
| Tests     | `cargo test`                  | all pass (88 today + new) |
| One test  | `cargo test healthz`          | the new test(s) pass      |
| Format    | `cargo fmt -- --check`        | exit 0, no diff           |
| Smoke     | `curl -s localhost:9812/healthz` | JSON with `"status":"ok"` |

## Scope

**In scope** (the only files you should modify):
- `src/routes.rs` — `Metrics` struct, two new `AppState` fields, the
  `/healthz` route + handler, the request-counter bump, a unit test.
- `src/main.rs` — construct the new `AppState` fields.
- `plans/README.md` — status row only.

**Out of scope** (do NOT touch):
- `src/cookies.rs` — read its existing public methods; add nothing.
- `src/fetch.rs`, `src/embed_cache.rs`, `src/notifier.rs`.
- **No Prometheus / `/metrics` exposition format** — JSON only this pass (a
  Prometheus text endpoint is a follow-up; see Maintenance notes).
- **No authentication layer** — the endpoint exposes only labels + booleans;
  gating is the reverse proxy's job (documented in Maintenance notes). Do not
  add an auth middleware here.

## Git workflow

- Branch: `advisor/013-healthz`
- Commit style: short imperative subject, no Conventional Commits prefix (e.g.
  "Add /healthz endpoint with counters and cookie state").
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add a `Metrics` struct

In `src/routes.rs`, add near `AppState`:

```rust
#[derive(Default)]
pub struct Metrics {
    pub requests: std::sync::atomic::AtomicU64,
    pub errors: std::sync::atomic::AtomicU64,
}
```

(Two counters is enough for v1. Keep it minimal.)

### Step 2: Add `metrics` and `started_at` to `AppState`

```rust
pub metrics: Arc<Metrics>,
pub started_at: std::time::Instant,
```

`Instant` is `Copy` and `AppState` is `Clone`, so this stays cheap. Add both
after `embed_cache`.

**Verify**: `cargo build` → fails only at the `AppState { … }` literal in
`main.rs`. Expected.

### Step 3: Construct the new fields in `main.rs`

In the `AppState { … }` literal (`src/main.rs:131-139`):

```rust
metrics: Arc::new(crate::routes::Metrics::default()),
started_at: std::time::Instant::now(),
```

**Verify**: `cargo build` → exit 0.

### Step 4: Count requests and errors

- At the **top of `process`** (`src/routes.rs:405`), bump the request counter:
  ```rust
  state.metrics.requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
  ```
- At the point where an **error embed `Response` is produced** (the error/`render`
  failure branch inside `process` — search `process` and its helpers for where a
  `FacebedError` becomes a response, e.g. an `error_response`/`format_error_embed`
  call), bump:
  ```rust
  state.metrics.errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
  ```
  Counts are best-effort observability, not load-bearing logic — if the exact
  error branch is ambiguous, place the increment where `format_error_embed` /
  `format_timeout_embed` results are returned and note it in your report. **If
  `process` has no identifiable error branch at all (drift), STOP and report.**

**Verify**: `cargo build` → exit 0.

### Step 5: Add the `/healthz` route and handler

Register the route in `router` (before the catch-all):

```rust
.route("/healthz", get(healthz))
```

Add the handler. It snapshots counters + the live cookie jar and returns JSON:

```rust
async fn healthz(State(state): State<AppState>) -> Response {
    use std::sync::atomic::Ordering::Relaxed;
    let jar = state.ctx.cookies.load();
    let accounts: Vec<serde_json::Value> = (0..jar.len())
        .map(|i| {
            serde_json::json!({
                "label": jar.label_at(i).unwrap_or("?"),
                "in_cooldown": jar.in_cooldown(i),
            })
        })
        .collect();
    let body = serde_json::json!({
        "status": "ok",
        "uptime_secs": state.started_at.elapsed().as_secs(),
        "requests": state.metrics.requests.load(Relaxed),
        "errors": state.metrics.errors.load(Relaxed),
        "cookie_accounts": jar.len(),
        "accounts": accounts,
    })
    .to_string();
    json_response(body)
}
```

`State`, `get`, `json_response`, `Response` are already imported. Do **not** add
a cache-control header — health checks should not be cached.

**Verify**: `cargo build` → exit 0; `cargo fmt -- --check` → exit 0.

### Step 6: Unit test the JSON shape

Refactor the body-building into a small pure helper so it's testable without a
server (mirrors how `build_oembed_json` is split out from the `oembed` handler):

```rust
fn build_healthz_json(uptime_secs: u64, requests: u64, errors: u64,
    accounts: &[(String, bool)]) -> String {
    let accounts: Vec<serde_json::Value> = accounts.iter()
        .map(|(label, cd)| serde_json::json!({"label": label, "in_cooldown": cd}))
        .collect();
    serde_json::json!({
        "status": "ok", "uptime_secs": uptime_secs,
        "requests": requests, "errors": errors,
        "cookie_accounts": accounts.len(), "accounts": accounts,
    }).to_string()
}
```

Have `healthz` call `build_healthz_json(...)`. Then test:

```rust
#[test]
fn healthz_json_reports_counters_and_accounts() {
    let s = build_healthz_json(42, 7, 1, &[("primary".into(), true), ("alt".into(), false)]);
    assert!(s.contains("\"status\":\"ok\""));
    assert!(s.contains("\"uptime_secs\":42"));
    assert!(s.contains("\"requests\":7"));
    assert!(s.contains("\"errors\":1"));
    assert!(s.contains("\"cookie_accounts\":2"));
    assert!(s.contains("\"label\":\"primary\""));
    assert!(s.contains("\"in_cooldown\":true"));
}
```

**Verify**: `cargo test healthz` → passes. `cargo test` → all pass.

## Test plan

- New test `healthz_json_reports_counters_and_accounts` in the `src/routes.rs`
  tests module — asserts every field of the health JSON. Models after the oEmbed
  tests added by plan 007 (which test `build_oembed_json` output directly).
- Manual smoke: `cargo run -- -c config.yaml` then
  `curl -s localhost:9812/healthz` → JSON with `status`, `uptime_secs`,
  `requests`, `accounts`.

## Done criteria

ALL must hold:

- [ ] `cargo build` exits 0
- [ ] `cargo fmt -- --check` exits 0 (no diff)
- [ ] `cargo test` exits 0; `healthz_json_reports_counters_and_accounts` passes
- [ ] `grep -n '"/healthz"' src/routes.rs` shows the route registered
- [ ] `grep -n "build_healthz_json" src/routes.rs` shows the helper used by the
      handler and the test
- [ ] `grep -n "metrics:\|started_at:" src/main.rs` shows both fields constructed
- [ ] The health JSON contains **no** cookie cookie-string/value field — only
      `label` + `in_cooldown` per account (verify by reading the handler)
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report (do not improvise) if:

- The drift check shows `src/routes.rs`/`src/main.rs` changed and excerpts no
  longer match.
- `process` has no identifiable error branch to attach the error counter to.
- `CookieJar` no longer exposes `len`/`label_at`/`in_cooldown` with the
  signatures in "Current state".
- Building the JSON would require a new dependency (it should not — `serde_json`
  is already present).

## Maintenance notes

For whoever owns this next:

- **Exposure**: `/healthz` reveals operator-chosen account labels and cooldown
  booleans — low sensitivity, but still internal. Gate it at the reverse proxy
  (e.g. nginx `location = /healthz { allow 127.0.0.1; deny all; }`) if you don't
  want it public. The endpoint deliberately exposes no cookie values, IDs, or URLs.
- **Counters reset on restart** (in-process atomics). That's fine for a liveness/
  at-a-glance view; if you need durable metrics, the follow-up is a Prometheus
  `/metrics` text endpoint (deferred this pass) scraped into a TSDB.
- **Reviewer**: confirm no secret leaks into the JSON, and that the request
  counter increments once per dynamic request (not per retry inside `fetch_until`).
- Composes with plan 012 (`banned_users` reload) and plan 011 (the in-flight cap
  could later be surfaced here as a `inflight`/`rejected` counter — a natural
  small extension).
