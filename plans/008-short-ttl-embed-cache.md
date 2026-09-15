# Plan 008: Add a short-TTL in-memory cache for rendered embeds

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat cbed1e3..HEAD -- src/routes.rs src/main.rs src/fetch.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition. Note: plans 001/003/004 also edit
> `src/routes.rs` — if they have landed, the line numbers below will have
> shifted; match on the code shape, not the exact line.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW-MED
- **Depends on**: none (soft: cleaner to land *after* the `src/routes.rs`-touching
  bug plans 004/001/003, to avoid drift churn — but not required)
- **Category**: direction
- **Planned at**: commit `cbed1e3`, 2026-06-17

## Why this matters

Every crawler hit re-fetches Facebook from scratch. When a popular link is
posted in several Discord servers, each server's crawler triggers its own full
scrape — same post, same render, seconds apart. There is already a precedent for
caching in this codebase (`MediaSizeCache`, the video-HEAD cache in
`src/fetch.rs:85-127`), but nothing caches the **rendered embed**.

A small in-memory TTL cache keyed by the post path does two things at once:
cuts p50 latency for repeat links to near-zero, **and** reduces outbound
Facebook request volume — which directly eases the rate-limit / checkpoint
pressure that plan 003 handles reactively. The cost is staleness: reaction
counts or edits within the TTL window are not reflected. Keeping the TTL short
(90 s) makes that acceptable for a link-preview service.

## Current state

Files involved:

- `src/fetch.rs:85-127` — the **pattern to copy**: `MediaSizeCache` (a
  TTL + size-bounded `HashMap`). Do not modify it; mirror it.
- `src/routes.rs` — `AppState` (where the cache is stored) and `process()`
  (where the cache is read and written).
- `src/main.rs` — builds `AppState`; you add the cache field's initializer.

### The pattern to mirror — `MediaSizeCache` (`src/fetch.rs:82-127`)

```rust
const VIDEO_HEAD_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const VIDEO_HEAD_CACHE_MAX: usize = 256;

#[derive(Default)]
struct MediaSizeCache {
    entries: HashMap<String, CachedContentLength>,
}

#[derive(Clone, Copy)]
struct CachedContentLength {
    value: Option<u64>,
    checked_at: Instant,
}

impl MediaSizeCache {
    fn get(&mut self, url: &str, now: Instant) -> Option<Option<u64>> {
        let Some(entry) = self.entries.get(url).copied() else {
            return None;
        };
        if now.duration_since(entry.checked_at) <= VIDEO_HEAD_CACHE_TTL {
            return Some(entry.value);
        }
        self.entries.remove(url);
        None
    }

    fn insert(&mut self, url: &str, value: Option<u64>, now: Instant) {
        if self.entries.len() >= VIDEO_HEAD_CACHE_MAX && !self.entries.contains_key(url) {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.checked_at)
                .map(|(url, _)| url.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            url.to_owned(),
            CachedContentLength { value, checked_at: now },
        );
    }
}
```

Its test (`src/fetch.rs:1164-1186`) is the structural pattern for your test.

### `AppState` (`src/routes.rs:29-35`)

```rust
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub ctx: Arc<ParserCtx>,
    pub notifier: Notifier,
    pub fetcher: Arc<Fetcher>,
}
```

`routes.rs` already imports `std::sync::Arc` (`src/routes.rs:24`) and
`std::time::{Duration, Instant}` (`src/routes.rs:25`).

### `process()` — the read/write sites (`src/routes.rs:361-471`)

`process()` runs the cookie-account retry loop and, on success, builds the body
and returns it. The relevant success tail is `src/routes.rs:416-437`:

```rust
            Ok(post) => {
                let scrape_ms = attempt_started.elapsed().as_millis();
                if n > 0 {
                    state.ctx.cookies.mark_ok(account_index);
                    if let Some(k) = key.as_deref() {
                        state.ctx.cookies.set_affinity(k.to_string(), account_index);
                    }
                }
                let render_started = Instant::now();
                let body = render_with_size_check(state, &post, kind).await;
                info!( /* ... */ "embed rendered");
                return html_response(body);
            }
```

`process()` is the single choke point for all successful renders: it is called
both from `process_with_deadline` (the normal path) and directly for the
`?type=3` Photocom path (`src/routes.rs:177`). Caching here covers every embed.
The `path` argument is the already-cleaned dispatch path — a good cache key.

### `AppState` construction in `src/main.rs:101-106`

```rust
    let state = AppState {
        config: Arc::new(config),
        ctx,
        notifier,
        fetcher,
    };
```

## Commands you will need

| Purpose        | Command                          | Expected on success        |
|----------------|----------------------------------|----------------------------|
| Build          | `cargo build`                    | exit 0                     |
| Tests          | `cargo test`                     | all pass (incl. new test)  |
| Targeted test  | `cargo test embed_cache`         | new test passes            |
| Format check   | `cargo fmt --check`              | exit 0, no diff            |
| Format apply   | `cargo fmt`                      | rewrites in place          |

CI parity: `cargo test --locked` and `cargo fmt --check`.

## Scope

**In scope** (the only files you should modify):
- `src/embed_cache.rs` — **new file**, the `EmbedCache` struct + test.
- `src/main.rs` — add `mod embed_cache;` and the `embed_cache` field initializer.
- `src/routes.rs` — add the `embed_cache` field to `AppState`; read at the top of
  `process()` and write on the success branch.

**Out of scope** (do NOT touch):
- `src/fetch.rs` and `MediaSizeCache` — copy the pattern, do not edit the original.
- The human-redirect (301) path, error embeds, and timeout embeds — only
  successful renders are cached.
- `render_with_size_check` / `render` internals.
- Any cache invalidation/eviction beyond the TTL + size bound copied from `MediaSizeCache`.

## Git workflow

- Branch: `advisor/008-short-ttl-embed-cache`.
- Commit style: short imperative subject (e.g. `Cache rendered embeds briefly`).
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Create `src/embed_cache.rs`

```rust
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long a rendered embed body is reused before re-scraping. Short on
/// purpose: reaction counts / edits within this window are not reflected.
pub const EMBED_CACHE_TTL: Duration = Duration::from_secs(90);
/// Max distinct cached paths. Past this, the oldest entry is evicted on insert.
pub const EMBED_CACHE_MAX: usize = 512;

#[derive(Default)]
pub struct EmbedCache {
    entries: HashMap<String, CachedEmbed>,
}

struct CachedEmbed {
    body: String,
    stored_at: Instant,
}

impl EmbedCache {
    /// Return a cached body for `key` if present and not expired. Expired
    /// entries are removed on access.
    pub fn get(&mut self, key: &str, now: Instant) -> Option<String> {
        let Some(entry) = self.entries.get(key) else {
            return None;
        };
        if now.duration_since(entry.stored_at) <= EMBED_CACHE_TTL {
            return Some(entry.body.clone());
        }
        self.entries.remove(key);
        None
    }

    pub fn insert(&mut self, key: &str, body: String, now: Instant) {
        if self.entries.len() >= EMBED_CACHE_MAX && !self.entries.contains_key(key) {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.stored_at)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            key.to_owned(),
            CachedEmbed { body, stored_at: now },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_cache_expires_and_bounds_entries() {
        let mut cache = EmbedCache::default();
        let now = Instant::now();

        cache.insert("groups/1/posts/2", "<html>A</html>".into(), now);
        assert_eq!(
            cache.get("groups/1/posts/2", now + Duration::from_secs(1)),
            Some("<html>A</html>".into())
        );
        assert_eq!(
            cache.get(
                "groups/1/posts/2",
                now + EMBED_CACHE_TTL + Duration::from_secs(1)
            ),
            None
        );

        for i in 0..=EMBED_CACHE_MAX {
            cache.insert(&format!("p/{i}"), "x".into(), now);
        }
        assert!(cache.entries.len() <= EMBED_CACHE_MAX);
    }
}
```

**Verify**: `cargo test embed_cache` → the new test passes. (`cargo build` will
warn that `EmbedCache` is unused until Step 3 — that is expected.)

### Step 2: Register the module and wire `AppState`

In `src/main.rs`, add to the `mod` block (`src/main.rs:8-18`):

```rust
mod embed_cache;
```

Add the field to `AppState` in `src/routes.rs:29-35`:

```rust
    pub embed_cache: Arc<std::sync::Mutex<crate::embed_cache::EmbedCache>>,
```

Initialize it in `src/main.rs:101-106`:

```rust
    let state = AppState {
        config: Arc::new(config),
        ctx,
        notifier,
        fetcher,
        embed_cache: Arc::new(std::sync::Mutex::new(crate::embed_cache::EmbedCache::default())),
    };
```

**Verify**: `cargo build` → exit 0.

### Step 3: Read and write the cache in `process()` (`src/routes.rs`)

At the **very top** of `process()` (before the cookie-ordering logic, near
`src/routes.rs:370`), add a cache check:

```rust
    {
        let now = Instant::now();
        if let Ok(mut cache) = state.embed_cache.lock() {
            if let Some(body) = cache.get(path, now) {
                info!(path = %path, cached = true, "embed cache hit");
                return html_response(body);
            }
        }
    }
```

On the **success branch** (`src/routes.rs:416-437`), insert into the cache before
returning. Change the body line so the cache gets a copy:

```rust
                let render_started = Instant::now();
                let body = render_with_size_check(state, &post, kind).await;
                if let Ok(mut cache) = state.embed_cache.lock() {
                    cache.insert(path, body.clone(), Instant::now());
                }
                info!( /* ...existing fields... */ "embed rendered");
                return html_response(body);
```

Only this success branch writes the cache — error, timeout, and banned paths are
never cached.

**Verify**: `cargo build` → exit 0; `cargo test` → all pass; `cargo fmt --check`
→ exit 0 (run `cargo fmt` first if needed).

### Step 4: Smoke-test the cache hit path manually

1. `cargo run`.
2. `curl -A 'Discordbot/2.0' 'http://127.0.0.1:9812/<a-real-fb-path>'` twice in
   a row.
3. In the server logs, the second request should show `embed cache hit`
   (`cached=true`) and return without an `embed rendered` line.

## Test plan

- New unit test `embed_cache_expires_and_bounds_entries` in `src/embed_cache.rs`,
  modeled on `media_size_cache_expires_and_bounds_entries` (`src/fetch.rs:1164-1186`):
  covers a fresh hit, TTL expiry, and the size bound.
- The cache-in-`process()` wiring is integration behavior verified by the Step 4
  manual smoke test (the existing suite has no HTTP-level harness — do not add
  one here).
- Verification: `cargo test` → all pass including the new test.

## Done criteria

ALL must hold:

- [ ] `cargo build` exits 0.
- [ ] `cargo test` exits 0; `embed_cache_expires_and_bounds_entries` passes.
- [ ] `cargo fmt --check` exits 0.
- [ ] `grep -n "embed cache hit" src/routes.rs` returns 1 match.
- [ ] `grep -n "cache.insert(path" src/routes.rs` returns 1 match (success branch only).
- [ ] `src/fetch.rs` is unmodified (`git diff --stat -- src/fetch.rs` empty).
- [ ] No files outside the in-scope list are modified.
- [ ] `plans/README.md` status row for 008 updated.

## STOP conditions

Stop and report (do not improvise) if:

- The "Current state" excerpts do not match the live code (drift). In
  particular, if plans 001/003/004 have rewritten `process()` such that the
  success branch no longer has a single `let body = render_with_size_check(...)`
  line, re-read `process()` and report the new shape before editing.
- You find yourself needing to cache error/timeout/redirect responses to make a
  test pass — that is out of scope; only successful renders are cached.
- A test requires spinning up an HTTP server — do not add an integration harness;
  rely on the unit test + manual smoke test.

## Maintenance notes

- `EMBED_CACHE_TTL` (90 s) is the staleness knob. If users complain that
  reaction counts lag, lower it; if Facebook rate-limits are the bigger pain,
  raise it. It interacts with plan 003 (cookie cooldowns): a longer TTL means
  fewer Facebook hits and less cooldown pressure.
- The cache key is the cleaned dispatch `path`. If a future change makes two
  distinct posts clean to the same path (they should not), they would collide —
  a reviewer should confirm `url_clean::clean_path` stays injective on post identity.
- Banned-user embeds are deterministic and currently flow through `process()`
  too, so they will be cached — harmless. If `banned_users` is edited at runtime
  in future, the cache TTL bounds how long a stale ban/unban lingers.
