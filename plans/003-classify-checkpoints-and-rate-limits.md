# Plan 003: Detect checkpoints and rate limits on the fetch path and cool down by cause

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat 3b6521c..HEAD -- src/error.rs src/fetch.rs src/cookies.rs src/routes.rs`
> Note: this plan assumes **plan 001 has already landed** (it changed
> `fetch_until`). If 001 is not yet DONE, stop and do 001 first. For every file,
> compare the "Current state" excerpts below against the live code before
> editing; on a mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M-L
- **Risk**: MED
- **Depends on**: plans/001-fix-fetch-until-quadratic-scan.md (both edit `fetch_until`; 001 must land first)
- **Category**: bug / resilience
- **Planned at**: commit `3b6521c`, 2026-06-17

## Why this matters

When facebed scrapes with cookies, Facebook responds to "too much, too fast"
with two distinct signals: **HTTP 429 rate limits** (transient — the account is
fine, just slow down) and **checkpoint / account-recovery redirects** (a hard
block that needs a human to re-export the cookie). Today the content-fetch path
(`Fetcher::fetch` / `fetch_until`) reads `resp.status()` only for logging and
never branches on it, and the checkpoint detector
(`cookie_probe_blocked_reason`) only runs at startup, never per request. So a
429 page and a checkpoint redirect both flow into HTML parsing, fail to yield a
post, and surface as a generic `Parse`/`NoData` error. The retry loop then gives
every such failure the **same flat 300-second cooldown** and counts it toward
the 3-strikes "cookie likely expired" Discord alert.

Concrete cost:
- A merely rate-limited account is benched for 5 minutes and falsely flagged as
  expired (admin gets paged to re-export a cookie that is fine).
- A genuinely checkpointed account is retried again every 5 minutes,
  re-tripping the checkpoint and deepening the block.
- A 429 currently classifies as `Parse` ("P"), so Facebook's 429 page is posted
  to the Discord webhook as a "parser bug" — webhook spam.

After this plan: a 429/503 yields a short cooldown and does **not** count the
account as bad; a checkpoint/recover yields a long cooldown and is treated as an
account-health failure; both render to the user as the existing "C" embed (no
new user-visible error letters, no webhook spam). Account rotation under load
becomes correct instead of self-defeating.

## Current state

Four files change. Excerpts are the live code today (post-001 for `fetch_until`).

### `src/error.rs` — error enum (lines 3-29) and `error_code` (52-59)

```rust
#[derive(Debug, Error)]
pub enum FacebedError {
    #[error("no data: {0}")]
    NoData(String),

    #[error("parse: {message}")]
    Parse {
        message: String,
        html: Option<String>,
        url: Option<String>,
    },

    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("other: {0}")]
    Other(#[from] anyhow::Error),
}
```

```rust
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::NoData(_) => "C",
            Self::Parse { .. } => "P",
            Self::Http(_) | Self::Io(_) | Self::Json(_) | Self::Yaml(_) => "U",
            Self::Other(_) => "X",
        }
    }
```

Constraint from `AGENTS.md` ("What NOT to do") and `README.md` ("Maintainer
notes"): **do not add or rename user-visible error letters.** The set is exactly
`C`, `P`, `U`, `X`, `T`. The two new variants must therefore map to an existing
letter — they map to `C`.

### `src/fetch.rs` — `fetch` (lines 318-332)

```rust
    /// Fetch a Facebook path. Optionally attach cookies. Raises NoData on login walls.
    pub async fn fetch(&self, post_path: &str, use_cookies: bool) -> FacebedResult<FetchedPage> {
        let started = Instant::now();
        let url = ensure_absolute(post_path);
        let (req, account_label) = self.request_for(&url, use_cookies);
        let resp = req.send().await?;
        let response_ms = started.elapsed().as_millis();
        let status = resp.status();
        let final_url = resp.url().to_string();
        let read_started = Instant::now();
        let html = resp.text().await?;
        let read_ms = read_started.elapsed().as_millis();
        tracing::info!(path = %post_path, account = %account_label, status = %status, final_url = %final_url, len = html.len(), partial = false, response_ms, read_ms, total_ms = started.elapsed().as_millis(), "fetch done");
        self.page_from_html(url, html, post_path, false)
    }
```

### `src/fetch.rs` — `fetch_until` (post-001 shape)

After plan 001, the head of `fetch_until` reads:

```rust
        let mut resp = req.send().await?;
        let response_ms = started.elapsed().as_millis();
        let status = resp.status();
        let final_url = resp.url().to_string();
        let mut body = Vec::new();
        let mut stopped_early = false;
        let read_started = Instant::now();
        while let Some(chunk) = resp.chunk().await? {
```

(If `fetch_until` does not look like this, plan 001 has not landed — STOP.)

Note: login-wall detection lives in `check_or_raise` → `probe_page_type`
(fetch.rs:874-910), called inside `page_from_html`. That stays as-is and keeps
mapping login walls to `NoData`. This plan adds classification that runs
**before** `page_from_html`, so a 429/checkpoint short-circuits before the
login-wall logic.

### `src/cookies.rs` — cooldown constants (60-76), struct (79-94), methods

```rust
pub const ACCOUNT_COOLDOWN_SECS: u64 = 300;
```

```rust
#[derive(Debug)]
pub struct CookieJar {
    accounts: Vec<CookieAccount>,
    /// Per-account "last failure" unix seconds. Parallel to `accounts`.
    /// Zero means "never failed".
    last_failures: Vec<AtomicU64>,
    consecutive_failures: Vec<AtomicU64>,
    affinity: Mutex<HashMap<String, usize>>,
}
```

`last_failures` is referenced in: the struct (line ~84), `empty()` (line ~101),
`load()` (line ~204), `mark_failed` (line ~321), `mark_ok` (line ~332),
`in_cooldown` (line ~351). Current method bodies:

```rust
    pub fn mark_failed(&self, i: usize) -> u64 {
        if self.accounts.is_empty() {
            return 0;
        }
        let idx = i % self.accounts.len();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.last_failures[idx].store(now, Ordering::Relaxed);
        self.consecutive_failures[idx].fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn mark_ok(&self, i: usize) {
        if self.accounts.is_empty() {
            return;
        }
        let idx = i % self.accounts.len();
        self.last_failures[idx].store(0, Ordering::Relaxed);
        self.consecutive_failures[idx].store(0, Ordering::Relaxed);
    }

    pub fn in_cooldown(&self, i: usize) -> bool {
        if self.accounts.is_empty() {
            return false;
        }
        let last = self.last_failures[i % self.accounts.len()].load(Ordering::Relaxed);
        if last == 0 {
            return false;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_sub(last) < ACCOUNT_COOLDOWN_SECS
    }
```

### `src/routes.rs` — `is_retryable` (524-530), failure handling in `process` (432-463), `maybe_notify_bad_account` (582-599)

```rust
fn is_retryable(e: &FacebedError) -> bool {
    match e {
        FacebedError::NoData(_) | FacebedError::Parse { .. } => true,
        FacebedError::Http(err) => err.is_timeout() || err.is_connect(),
        _ => false,
    }
}
```

The two failure branches inside the `for (loop_idx, &account_index) in order...`
loop:

```rust
            Err(e) if is_retryable(&e) && loop_idx + 1 < order.len() => {
                if n > 0 {
                    let count = state.ctx.cookies.mark_failed(account_index);
                    maybe_notify_bad_account(state, account_index, count, &e);
                    if let Some(k) = key.as_deref() {
                        if state.ctx.cookies.affinity_for(k) == Some(account_index) {
                            state.ctx.cookies.forget_affinity(k);
                        }
                    }
                }
                let label = state.ctx.cookies.label_at(account_index).unwrap_or("?");
                warn!(path = %path, attempt = loop_idx, account = %label, error = %e, elapsed_ms = attempt_started.elapsed().as_millis(), "retrying with fallback account");
                last_err = Some(e);
                continue;
            }
            Err(e) => {
                if n > 0 {
                    let count = state.ctx.cookies.mark_failed(account_index);
                    maybe_notify_bad_account(state, account_index, count, &e);
                }
                warn!(
                    path = %path,
                    kind = ?kind,
                    account = %state.ctx.cookies.label_at(account_index).unwrap_or(""),
                    attempt = loop_idx,
                    error = %e,
                    attempt_ms = attempt_started.elapsed().as_millis(),
                    total_ms = process_started.elapsed().as_millis(),
                    "embed render failed"
                );
                return error_response(state, path, e);
            }
```

`maybe_notify_bad_account` fires the Discord alert only when the consecutive
count equals `NOTIFY_FAILURE_THRESHOLD` (3), then resets the counter.
`error_response` (601-635) matches `NoData`/`Parse`/`_`; the `_` arm warns and
renders the embed with no webhook post — so the new variants will render `"C"`
with no webhook spam, which is the desired behavior.

### Repo conventions to follow

- Errors are a single `FacebedError` enum (`src/error.rs`); construct via the
  associated fns (`FacebedError::no_data`, `::parse`, …). Add the new ones the
  same way.
- Cooldown/affinity bookkeeping lives entirely in `CookieJar` (`src/cookies.rs`)
  using `AtomicU64`/`Ordering::Relaxed` (it is a hint, not a hard invariant —
  see the doc comments). Match that.
- Tests live in `#[cfg(test)] mod tests` at the bottom of each file. `error.rs`
  has none today (add one); `fetch.rs`, `cookies.rs`, `routes.rs` already do.
- Hard gates: `cargo fmt --check` and `cargo test`.

## Commands you will need

| Purpose      | Command                 | Expected on success    |
|--------------|-------------------------|------------------------|
| Build        | `cargo build`           | exit 0                 |
| Tests        | `cargo test --locked`   | all pass               |
| Format       | `cargo fmt`             | reformats in place     |
| Format check | `cargo fmt --check`     | exit 0                 |

(Exact CI gates from `.github/workflows/docker-build.yml`.)

## Scope

**In scope** (the only files you may modify):
- `src/error.rs` — add `RateLimited` and `Checkpointed` variants + constructors + `error_code` arms + a test.
- `src/fetch.rs` — add `classify_block` and `retry_after_secs`; call them in `fetch` and `fetch_until`; add a test.
- `src/cookies.rs` — rename `last_failures` → `cooldown_until` (semantics change to "unix secs when cooldown ends"); add `mark_rate_limited`, `mark_checkpointed`; add tests.
- `src/routes.rs` — extend `is_retryable`; add `record_account_failure`; use it in both failure branches; add a test.

**Out of scope** (do NOT touch):
- `check_cookie_account` / `cookie_probe_blocked_reason` (the startup probe) —
  it already classifies checkpoints; leave it.
- `resolve_share_link*` (share resolution) — classifying blocks there is a
  deliberate follow-up (see Maintenance notes), not this plan.
- `probe_page_type` / `check_or_raise` / login-wall handling — unchanged.
- `head_content_length` and the media-size cache — unrelated.
- The error-letter set: do NOT add a new letter; map new variants to `"C"`.

## Git workflow

- Branch: `advisor/003-block-classification`.
- Commit style: short imperative subject (match `git log --oneline`, e.g.
  "Cool down by failure cause for cookie accounts"). Consider one commit per
  step.
- Do NOT push or open a PR unless the operator instructs it.

## Steps

### Step 1: Add the two error variants (`src/error.rs`)

Add to the `FacebedError` enum (after the `Other` variant):

```rust
    #[error("rate limited")]
    RateLimited { retry_after: Option<u64> },

    #[error("checkpoint")]
    Checkpointed,
```

Add constructors in the `impl FacebedError` block:

```rust
    pub fn rate_limited(retry_after: Option<u64>) -> Self {
        Self::RateLimited { retry_after }
    }

    pub fn checkpointed() -> Self {
        Self::Checkpointed
    }
```

Extend `error_code` so both map to `"C"`:

```rust
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::NoData(_) | Self::RateLimited { .. } | Self::Checkpointed => "C",
            Self::Parse { .. } => "P",
            Self::Http(_) | Self::Io(_) | Self::Json(_) | Self::Yaml(_) => "U",
            Self::Other(_) => "X",
        }
    }
```

Add a test module at the bottom of `src/error.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::FacebedError;

    #[test]
    fn rate_limit_and_checkpoint_render_as_c() {
        assert_eq!(FacebedError::rate_limited(Some(30)).error_code(), "C");
        assert_eq!(FacebedError::checkpointed().error_code(), "C");
    }
}
```

**Verify**: `cargo build` → may fail in `src/routes.rs` only (the `is_retryable`
match is now non-exhaustive against new variants if it had no wildcard — it does
have `_ => false`, so it should still compile). `cargo test --locked error` →
the new test passes.

### Step 2: Add response classification in `src/fetch.rs`

Add these two free functions near the other free functions in `fetch.rs` (e.g.
just above `probe_page_type`):

```rust
/// Classify a Facebook response that indicates the request was blocked rather
/// than served. Returns the corresponding error, or `None` for a normal
/// response that should be parsed. Rate limits (429/503) are transient; a
/// redirect to /checkpoint or /recover means the account needs a human to
/// re-export its cookie. Login walls are NOT handled here — they are detected
/// later by `check_or_raise` and mapped to `NoData`.
fn classify_block(
    status: reqwest::StatusCode,
    final_url: &str,
    retry_after: Option<u64>,
) -> Option<FacebedError> {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
    {
        return Some(FacebedError::rate_limited(retry_after));
    }
    let lower = final_url.to_ascii_lowercase();
    if lower.contains("/checkpoint") || lower.contains("/recover") {
        return Some(FacebedError::checkpointed());
    }
    None
}

/// Parse a `Retry-After` header expressed in whole seconds. HTTP-date form is
/// ignored (returns `None`); the caller falls back to a default cooldown.
fn retry_after_secs(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
}
```

Wire it into `fetch` — capture `retry_after` before the body is consumed, and
short-circuit before reading the body:

```rust
    pub async fn fetch(&self, post_path: &str, use_cookies: bool) -> FacebedResult<FetchedPage> {
        let started = Instant::now();
        let url = ensure_absolute(post_path);
        let (req, account_label) = self.request_for(&url, use_cookies);
        let resp = req.send().await?;
        let response_ms = started.elapsed().as_millis();
        let status = resp.status();
        let final_url = resp.url().to_string();
        let retry_after = retry_after_secs(&resp);
        if let Some(err) = classify_block(status, &final_url, retry_after) {
            tracing::warn!(path = %post_path, account = %account_label, status = %status, final_url = %final_url, response_ms, "fetch blocked");
            return Err(err);
        }
        let read_started = Instant::now();
        let html = resp.text().await?;
        let read_ms = read_started.elapsed().as_millis();
        tracing::info!(path = %post_path, account = %account_label, status = %status, final_url = %final_url, len = html.len(), partial = false, response_ms, read_ms, total_ms = started.elapsed().as_millis(), "fetch done");
        self.page_from_html(url, html, post_path, false)
    }
```

Wire it into `fetch_until` (post-001). Capture `retry_after` right after
`final_url`, and short-circuit **before** the chunk loop so a blocked response
is not downloaded:

```rust
        let mut resp = req.send().await?;
        let response_ms = started.elapsed().as_millis();
        let status = resp.status();
        let final_url = resp.url().to_string();
        let retry_after = retry_after_secs(&resp);
        if let Some(err) = classify_block(status, &final_url, retry_after) {
            tracing::warn!(path = %post_path, account = %account_label, status = %status, final_url = %final_url, response_ms, "fetch blocked");
            return Err(err);
        }
        let mut body = Vec::new();
        let mut stopped_early = false;
        let read_started = Instant::now();
        while let Some(chunk) = resp.chunk().await? {
```

(Leave the rest of `fetch_until` — the scan loop, decode, and `page_from_html`
call — unchanged.)

Add a test for `classify_block` inside the existing `#[cfg(test)] mod tests` in
`fetch.rs`:

```rust
    #[test]
    fn classify_block_flags_rate_limit_and_checkpoint() {
        use crate::error::FacebedError;
        use reqwest::StatusCode;
        assert!(matches!(
            super::classify_block(
                StatusCode::TOO_MANY_REQUESTS,
                "https://www.facebook.com/x",
                Some(30)
            ),
            Some(FacebedError::RateLimited { retry_after: Some(30) })
        ));
        assert!(matches!(
            super::classify_block(StatusCode::SERVICE_UNAVAILABLE, "https://www.facebook.com/x", None),
            Some(FacebedError::RateLimited { retry_after: None })
        ));
        assert!(matches!(
            super::classify_block(StatusCode::OK, "https://www.facebook.com/checkpoint/?next=y", None),
            Some(FacebedError::Checkpointed)
        ));
        assert!(super::classify_block(
            StatusCode::OK,
            "https://www.facebook.com/groups/1/posts/2",
            None
        )
        .is_none());
    }
```

**Verify**: `cargo build` → exit 0. `cargo test --locked fetch` → all fetch
tests pass including the new one.

### Step 3: Make `CookieJar` cooldown cause-aware (`src/cookies.rs`)

(a) Add cooldown constants next to `ACCOUNT_COOLDOWN_SECS` (after its doc
comment, around line 65):

```rust
/// Cooldown after a soft rate limit (HTTP 429/503). Short: FB rate limits are
/// usually per-minute and the cookie itself is healthy — we only need to slow
/// down. A `Retry-After` value overrides this, capped at the max below.
pub const RATE_LIMIT_COOLDOWN_SECS: u64 = 60;
pub const RATE_LIMIT_COOLDOWN_MAX_SECS: u64 = 600;

/// Cooldown after a checkpoint / account-recovery redirect. Long: a checkpoint
/// will not clear within minutes; it needs a human to re-export the cookie. A
/// long cooldown stops us re-tripping the checkpoint every few minutes.
pub const CHECKPOINT_COOLDOWN_SECS: u64 = 1800;
```

(b) Rename the field `last_failures` → `cooldown_until` in the struct, the
`#[cfg(test)] empty()` initializer, and the `load()` initializer. The new
semantics: it stores the unix-second timestamp at which the cooldown ends (0 =
no cooldown), instead of "last failure time". Update the struct doc comment to
say so.

(c) Add a private helper and rewrite the three methods. Replace the current
`mark_failed`, `mark_ok`, `in_cooldown` with:

```rust
    fn set_cooldown(&self, idx: usize, secs: u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.cooldown_until[idx].store(now.saturating_add(secs), Ordering::Relaxed);
    }

    pub fn mark_failed(&self, i: usize) -> u64 {
        if self.accounts.is_empty() {
            return 0;
        }
        let idx = i % self.accounts.len();
        self.set_cooldown(idx, ACCOUNT_COOLDOWN_SECS);
        self.consecutive_failures[idx].fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Soft rate limit: cool down briefly (honoring `Retry-After` when present,
    /// capped) and do NOT touch the consecutive-failure counter — a rate limit
    /// is not a bad cookie and must not trigger the "expired cookie" alert.
    pub fn mark_rate_limited(&self, i: usize, retry_after: Option<u64>) {
        if self.accounts.is_empty() {
            return;
        }
        let idx = i % self.accounts.len();
        let secs = retry_after
            .map(|s| s.min(RATE_LIMIT_COOLDOWN_MAX_SECS))
            .unwrap_or(RATE_LIMIT_COOLDOWN_SECS);
        self.set_cooldown(idx, secs);
    }

    /// Checkpoint / recovery block: long cooldown, and count it as a failure so
    /// the admin alert fires (the cookie needs human re-export).
    pub fn mark_checkpointed(&self, i: usize) -> u64 {
        if self.accounts.is_empty() {
            return 0;
        }
        let idx = i % self.accounts.len();
        self.set_cooldown(idx, CHECKPOINT_COOLDOWN_SECS);
        self.consecutive_failures[idx].fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn mark_ok(&self, i: usize) {
        if self.accounts.is_empty() {
            return;
        }
        let idx = i % self.accounts.len();
        self.cooldown_until[idx].store(0, Ordering::Relaxed);
        self.consecutive_failures[idx].store(0, Ordering::Relaxed);
    }

    pub fn in_cooldown(&self, i: usize) -> bool {
        if self.accounts.is_empty() {
            return false;
        }
        let until = self.cooldown_until[i % self.accounts.len()].load(Ordering::Relaxed);
        if until == 0 {
            return false;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now < until
    }
```

Leave `reset_failure_count`, `set_affinity`, `affinity_for`, `forget_affinity`,
`account_at`, `label_at`, `len` unchanged.

(d) Add tests to the existing `cookies.rs` `mod tests`:

```rust
    #[test]
    fn rate_limit_cools_down_without_marking_bad() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        fs::write(&main, r#"[{"name":"c_user","value":"1"}]"#).unwrap();
        let jar = CookieJar::load(&main).unwrap();

        jar.mark_rate_limited(0, None);
        assert!(jar.in_cooldown(0));
        // A rate limit must not count toward the consecutive-failure alert:
        // the next real failure is still the 1st in the streak.
        assert_eq!(jar.mark_failed(0), 1);
    }

    #[test]
    fn checkpoint_cools_down_and_counts_failures() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        fs::write(&main, r#"[{"name":"c_user","value":"1"}]"#).unwrap();
        let jar = CookieJar::load(&main).unwrap();

        assert_eq!(jar.mark_checkpointed(0), 1);
        assert!(jar.in_cooldown(0));
        assert_eq!(jar.mark_checkpointed(0), 2);
    }
```

**Verify**: `cargo build` → exit 0. `cargo test --locked cookies` → all cookies
tests pass, including the two new ones AND the pre-existing
`reset_failure_count_does_not_clear_cooldown` and
`mark_failed_returns_consecutive_count` (they must still pass — confirm).

### Step 4: Route failures by cause (`src/routes.rs`)

(a) Extend `is_retryable`:

```rust
fn is_retryable(e: &FacebedError) -> bool {
    match e {
        FacebedError::NoData(_)
        | FacebedError::Parse { .. }
        | FacebedError::RateLimited { .. }
        | FacebedError::Checkpointed => true,
        FacebedError::Http(err) => err.is_timeout() || err.is_connect(),
        _ => false,
    }
}
```

(b) Add a helper (place it next to `maybe_notify_bad_account`):

```rust
/// Apply the cooldown and alerting appropriate to a failed attempt's cause.
/// Rate limits cool down briefly and do NOT count the account as bad (so they
/// never fire the "expired cookie" alert) and keep their affinity pin.
/// Checkpoints get a long cooldown and count as failures. Everything else uses
/// the default failure cooldown. Checkpoints and generic failures also drop a
/// stale affinity pin for `key`.
fn record_account_failure(
    state: &AppState,
    account_index: usize,
    e: &FacebedError,
    key: Option<&str>,
) {
    match e {
        FacebedError::RateLimited { retry_after } => {
            state
                .ctx
                .cookies
                .mark_rate_limited(account_index, *retry_after);
            return;
        }
        FacebedError::Checkpointed => {
            let count = state.ctx.cookies.mark_checkpointed(account_index);
            maybe_notify_bad_account(state, account_index, count, e);
        }
        _ => {
            let count = state.ctx.cookies.mark_failed(account_index);
            maybe_notify_bad_account(state, account_index, count, e);
        }
    }
    if let Some(k) = key {
        if state.ctx.cookies.affinity_for(k) == Some(account_index) {
            state.ctx.cookies.forget_affinity(k);
        }
    }
}
```

(c) In the **retryable** failure branch, replace the inner `if n > 0 { ... }`
block (the `mark_failed` + `maybe_notify_bad_account` + affinity-forget) with:

```rust
                if n > 0 {
                    record_account_failure(state, account_index, &e, key.as_deref());
                }
```

(d) In the **terminal** failure branch, replace its `if n > 0 { let count = ...; maybe_notify_bad_account(...); }`
with the same call:

```rust
                if n > 0 {
                    record_account_failure(state, account_index, &e, key.as_deref());
                }
```

Leave the surrounding `warn!`/`last_err`/`continue`/`return error_response(...)`
lines in both branches unchanged.

(e) Add a test to the existing `routes.rs` `mod tests`:

```rust
    #[test]
    fn rate_limit_and_checkpoint_are_retryable() {
        use crate::error::FacebedError;
        assert!(super::is_retryable(&FacebedError::rate_limited(Some(30))));
        assert!(super::is_retryable(&FacebedError::checkpointed()));
    }
```

(The `tests` module's `use super::{...}` line may need `is_retryable` — instead
of editing it, the test above calls `super::is_retryable` directly.)

**Verify**: `cargo build` → exit 0. `cargo test --locked` → entire suite passes.

### Step 5: Format

Run `cargo fmt`, then confirm clean.

**Verify**: `cargo fmt --check` → exit 0.

## Test plan

- `src/error.rs`: `rate_limit_and_checkpoint_render_as_c` — both new variants
  map to `"C"` (keeps the user-visible letter set stable).
- `src/fetch.rs`: `classify_block_flags_rate_limit_and_checkpoint` — 429 ⇒
  `RateLimited` with the parsed `retry_after`; 503 ⇒ `RateLimited`; a
  `/checkpoint` final URL ⇒ `Checkpointed`; a normal post URL ⇒ `None`.
- `src/cookies.rs`: `rate_limit_cools_down_without_marking_bad` (cools down but
  the next `mark_failed` still returns 1 — proving no consecutive bump) and
  `checkpoint_cools_down_and_counts_failures` (cools down and bumps the streak).
  Pre-existing cooldown tests must still pass.
- `src/routes.rs`: `rate_limit_and_checkpoint_are_retryable`.
- Structural pattern: the existing `mod tests` in each of those files
  (`cookies.rs` uses `tempfile` + `fs::write`; follow it).
- Verification: `cargo test --locked` → all pass, including the new tests and
  every pre-existing test.

## Done criteria

ALL must hold:

- [ ] `cargo build` exits 0.
- [ ] `cargo test --locked` exits 0; all new tests pass and no pre-existing test
      regressed.
- [ ] `cargo fmt --check` exits 0.
- [ ] `grep -n "last_failures" src/cookies.rs` returns no matches (field fully
      renamed).
- [ ] `grep -rn "fn error_code" src/error.rs` shows `RateLimited`/`Checkpointed`
      mapping to `"C"`, and `grep -rn '"R"\|"T"' src/error.rs` shows no new
      error letter was introduced in `error_code`.
- [ ] `grep -n "classify_block" src/fetch.rs` shows it called in both `fetch`
      and `fetch_until`.
- [ ] `git status` shows only `src/error.rs`, `src/fetch.rs`, `src/cookies.rs`,
      `src/routes.rs` modified.
- [ ] `plans/README.md` status row for 003 updated.

## STOP conditions

Stop and report back (do not improvise) if:

- Plan 001 has not landed (`fetch_until` does not match the post-001 excerpt).
- The drift check shows any of the four files changed since `3b6521c` and the
  "Current state" excerpts no longer match.
- Renaming `last_failures` breaks `reset_failure_count_does_not_clear_cooldown`
  or `mark_failed_returns_consecutive_count` — that means the cooldown semantics
  changed in a way the existing tests catch; report rather than editing those
  tests to pass.
- You find a third match for `last_failures` outside the spots listed in
  "Current state" — report the extra reference before renaming.
- A change would require adding a new user-visible error letter (it must not) or
  touching `resolve_share_link*` / `check_cookie_account`.

## Maintenance notes

- **Deferred follow-up (intentional):** share-link resolution
  (`resolve_share_link_body` / `_head`) also fetches Facebook and is NOT covered
  here — a checkpoint during share resolution still slips through as an empty
  resolve. Extending `classify_block` into those functions is a sensible next
  step but was left out to bound this plan's blast radius.
- **Immediate checkpoint alerting:** a checkpointed account now alerts via the
  same 3-strikes threshold as generic failures. Because the checkpoint cooldown
  is long (30 min), three strikes take a while to accumulate, so the alert is
  slower than ideal. If faster paging matters, consider alerting on the first
  checkpoint (e.g. a dedicated notify in `record_account_failure`).
- **`error_response` logging:** the new variants fall through to the `_ =>
  "unclassified error"` arm in `error_response`. That is harmless (renders `"C"`,
  no webhook), but a reviewer may want explicit arms for cleaner logs.
- Reviewer should scrutinize: (1) `classify_block` runs before login-wall
  detection, so confirm a real login wall on a 200 response still becomes
  `NoData` (it does — `classify_block` returns `None` for 200 + non-checkpoint
  URL, and `check_or_raise` handles it downstream); (2) the `cooldown_until`
  rename didn't leave any reference reading it as "last failure time".
