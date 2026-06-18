# Plan 011: Cap concurrent Facebook fetches to protect the cookie pool

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: `git diff --stat 2e6610f..HEAD -- src/routes.rs src/main.rs`
> If either file changed since this plan was written, compare the "Current
> state" excerpts against the live code before proceeding; on a mismatch,
> treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S-M
- **Risk**: MED
- **Depends on**: none
- **Category**: direction
- **Planned at**: commit `2e6610f`, 2026-06-18

## Why this matters

`facebed` is a **public** endpoint that drives **cookied** Facebook fetches.
Anyone who learns your domain can fan thousands of requests at it; each one
spends a real Facebook request against your cookie accounts and your IP's
ban-budget. The existing protections are the wrong shape for this: `banned_users`
filters Facebook *authors* (`src/parsers/mod.rs:39`), and the plan-003 cooldown
(`mark_rate_limited` / `mark_checkpointed` in `src/cookies.rs`) is **reactive** —
it only kicks in *after* Facebook has already throttled or checkpointed you. There
is no cap on how many FB fetches are in flight at once: `grep -rn "Semaphore" src/`
returns nothing.

This plan adds one inbound control: a **global concurrency cap on the Facebook-
fetching path**. When more than `N` post-renders are in flight, additional
requests get a fast `503` instead of piling more concurrent load onto the cookie
pool. This bounds cookie burn under a traffic spike or a scraper, without adding
a heavyweight framework (which `AGENTS.md` forbids) and without a new crate.

Per-IP rate limiting is intentionally **out of scope** here — see "Maintenance
notes" for why and what to do if you want it.

## Current state

- `src/routes.rs` — all routing + the post-render path.
  - `AppState` (the shared, cloned-per-request state):
    ```rust
    // src/routes.rs:29-36
    #[derive(Clone)]
    pub struct AppState {
        pub config: Arc<Config>,
        pub ctx: Arc<ParserCtx>,
        pub notifier: Notifier,
        pub fetcher: Arc<Fetcher>,
        pub embed_cache: Arc<std::sync::Mutex<crate::embed_cache::EmbedCache>>,
    }
    ```
  - `process` is the single function every Facebook fetch goes through (the
    catch-all dispatches to it for every parser kind). Its signature and top:
    ```rust
    // src/routes.rs:405
    async fn process(state: &AppState, path: &str, kind: ParserKind) -> Response {
    ```
    (The embed-cache read happens near the top of `process`; the FB fetch happens
    below it via `process_with_deadline` → `run_parser`.)
  - `Response`, `StatusCode`, `HeaderMap`, `HeaderValue` are already imported
    (`src/routes.rs:17-21`). `std::sync::Arc` is imported (`src/routes.rs:24`).
- `src/main.rs` — builds `AppState` and starts the server:
  ```rust
  // src/main.rs:131-139
  let state = AppState {
      config: Arc::new(config),
      ctx,
      notifier,
      fetcher,
      embed_cache: Arc::new(std::sync::Mutex::new(
          crate::embed_cache::EmbedCache::default(),
      )),
  };
  ```
- **Convention to follow**: shared mutable/owned services live on `AppState`
  behind `Arc`, constructed in `main.rs` and read in `routes.rs`. The
  `embed_cache` field (added by plan 008) is the exemplar — copy its shape.
- **Concurrency primitive**: use `tokio::sync::Semaphore` (already available —
  `tokio` is a dependency with the `sync` feature, see `Cargo.toml`). No new
  crate.

## Commands you will need

| Purpose   | Command                          | Expected on success      |
|-----------|----------------------------------|--------------------------|
| Build     | `cargo build`                    | exit 0                   |
| Tests     | `cargo test`                     | all pass (88 today + new)|
| One test  | `cargo test fetch_cap`           | the new test passes      |
| Format    | `cargo fmt -- --check`           | exit 0, no diff          |

## Scope

**In scope** (the only files you should modify):
- `src/routes.rs` — add the semaphore field, the acquire in `process`, the busy
  response, and a unit test.
- `src/main.rs` — construct the semaphore into `AppState`.
- `plans/README.md` — status row only.

**Out of scope** (do NOT touch):
- `src/cookies.rs`, `src/fetch.rs` — the reactive cooldown is separate and
  correct; do not change it.
- `src/embed_cache.rs` — unrelated.
- Per-IP rate limiting / any new dependency in `Cargo.toml` — explicitly deferred.
- The semaphore must NOT wrap `/`, `/favicon.ico`, `/banner.png`, or
  `/oembed.json` — only the FB-fetch path inside `process`.

## Git workflow

- Branch: `advisor/011-fetch-cap`
- Commit style: short imperative subject, no Conventional Commits prefix (match
  `git log --oneline`, e.g. "Cap concurrent Facebook fetches"). Body only if the
  "why" isn't obvious.
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add a `fetch_limit` semaphore to `AppState`

In `src/routes.rs`, add a field to `AppState`:

```rust
pub fetch_limit: Arc<tokio::sync::Semaphore>,
```

Place it after `embed_cache`. Keep `#[derive(Clone)]` working — `Arc<Semaphore>`
is `Clone`, so nothing else changes.

**Verify**: `cargo build` → fails to compile **only** at the `AppState { … }`
literal in `src/main.rs` (missing field). That expected failure confirms the
field is wired. Proceed to Step 2.

### Step 2: Construct the semaphore in `main.rs`

In `src/main.rs`, define the cap as a `const` near the top of the file (after the
imports), and pass it into the `AppState` literal:

```rust
/// Max Facebook post-renders in flight at once. Excess requests get a fast 503
/// instead of piling concurrent load onto the cookie pool. Tune for your host.
const MAX_INFLIGHT_FETCHES: usize = 16;
```

```rust
let state = AppState {
    config: Arc::new(config),
    ctx,
    notifier,
    fetcher,
    embed_cache: Arc::new(std::sync::Mutex::new(
        crate::embed_cache::EmbedCache::default(),
    )),
    fetch_limit: Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_FETCHES)),
};
```

**Verify**: `cargo build` → exit 0.

### Step 3: Acquire a permit at the top of `process`, before any fetch

In `src/routes.rs`, inside `process`, acquire a permit **after** the embed-cache
read short-circuit (a cache hit is cheap and must not be throttled) but **before**
the path that actually fetches from Facebook (`process_with_deadline`).

Use `try_acquire` (non-blocking) so an overloaded server fails fast instead of
queueing unboundedly:

```rust
let _permit = match state.fetch_limit.clone().try_acquire_owned() {
    Ok(p) => p,
    Err(_) => return busy_response(),
};
```

Hold `_permit` for the rest of `process` — it auto-releases when `process`
returns. **Do not** drop it early.

If the embed-cache read is the very first thing in `process` and returns on hit,
put the acquire on the line immediately after that block. If you are unsure where
the cache-hit early-return is, search `process` for the `embed_cache` lock; the
acquire goes after the hit branch returns and before the first `.fetch`/
`process_with_deadline` call. **If `process` has no embed-cache read at the top
(drift), STOP and report.**

### Step 4: Add the `busy_response` helper

Add near the other response helpers (e.g. next to `json_response`,
`src/routes.rs:89`):

```rust
fn busy_response() -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::RETRY_AFTER,
        HeaderValue::from_static("2"),
    );
    (StatusCode::SERVICE_UNAVAILABLE, headers, "busy").into_response()
}
```

**Verify**: `cargo build` → exit 0; `cargo fmt -- --check` → exit 0.

### Step 5: Unit test the cap

Add a test in the existing `#[cfg(test)] mod tests` block in `src/routes.rs`
(there are already tests there — `video_routes_share_kind_affinity` at
`src/routes.rs:766` and `rate_limit_and_checkpoint_are_retryable` at
`src/routes.rs:772`; add yours alongside). Test the semaphore behavior directly
(do not stand up a server):

```rust
#[test]
fn fetch_cap_rejects_when_exhausted() {
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(2));
    let _a = sem.clone().try_acquire_owned().expect("first permit");
    let _b = sem.clone().try_acquire_owned().expect("second permit");
    assert!(
        sem.clone().try_acquire_owned().is_err(),
        "third acquire must fail when the 2-permit cap is exhausted"
    );
    drop(_a);
    assert!(
        sem.clone().try_acquire_owned().is_ok(),
        "a permit frees up after one is dropped"
    );
}
```

**Verify**: `cargo test fetch_cap` → 1 passed. `cargo test` → all pass.

## Test plan

- New test: `fetch_cap_rejects_when_exhausted` in `src/routes.rs` tests module —
  proves the permit math (exhaustion rejects, release re-admits). Models after the
  existing plain `#[test]` functions in that module.
- Manual smoke (optional, not required for done): start the server, fire ~30
  concurrent `curl -A 'Discordbot/2.0' localhost:9812/<valid-fb-path>` and confirm
  some return `503` with `Retry-After: 2` while the process stays up.

## Done criteria

ALL must hold:

- [ ] `cargo build` exits 0
- [ ] `cargo fmt -- --check` exits 0 (no diff)
- [ ] `cargo test` exits 0; `fetch_cap_rejects_when_exhausted` exists and passes
- [ ] `grep -n "try_acquire_owned" src/routes.rs` shows the acquire inside
      `process` AND the test
- [ ] `grep -n "fetch_limit" src/routes.rs src/main.rs` shows the field declared
      and constructed
- [ ] The semaphore is acquired only in `process`, not in `router`/`catch_all`
      around the static routes (verify by reading: `/`, `/favicon.ico`,
      `/banner.png`, `/oembed.json` handlers are untouched)
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report (do not improvise) if:

- The drift check shows `src/routes.rs` or `src/main.rs` changed and the
  "Current state" excerpts no longer match.
- `process` does not begin with an embed-cache read, so there's no clear point to
  put the acquire after the cache-hit return.
- Adding the field forces touching any file outside the in-scope list.
- Holding the permit for the body of `process` would require restructuring
  `process` substantially (e.g. it spawns the fetch onto another task) — report
  the structure instead of guessing.

## Maintenance notes

For whoever owns this next:

- **Tuning**: `MAX_INFLIGHT_FETCHES` (16) is a starting point. Raise it if your
  cookie pool is large and your host has headroom; lower it if you see FB
  throttling under load. It bounds *concurrent* FB fetches, not request rate.
- **Per-IP rate limiting was deliberately deferred.** It needs either a new crate
  (`tower_governor`) or a hand-rolled IP→token-bucket map, and — critically —
  correct client-IP extraction behind the reverse proxy the README assumes
  (nginx/Caddy/Cloudflare). Trusting `X-Forwarded-For` blindly lets a caller
  forge IPs. If you add it, do it at the reverse proxy first (nginx `limit_req`),
  which already sees the real peer IP; only fall back to app-level if you need
  per-route limits nginx can't express. A reviewer should reject any app-level
  XFF parsing that doesn't pin the trusted proxy hop.
- **Interaction**: this cap sits *in front of* the plan-008 embed cache miss path
  and the plan-003 cooldown. A cache hit never consumes a permit (by design —
  Step 3). If `process` is ever refactored to fetch before checking the cache,
  this plan's placement must be revisited.
