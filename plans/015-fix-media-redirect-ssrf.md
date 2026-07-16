# Plan 015: Close the redirect-hop SSRF on `/media`

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: `git diff --stat e68aa0a..HEAD -- src/routes.rs src/fetch.rs`
> If either changed since this plan was written, compare the "Current state"
> excerpts against the live code before proceeding; on a mismatch, treat it as a
> STOP condition.

## Status

- **Priority**: P1 (security — must land before `/media` is exposed in any deploy)
- **Effort**: S
- **Risk**: LOW
- **Depends on**: plan 014 (the `/media` route this hardens — landed in `e68aa0a`)
- **Category**: security
- **Planned at**: commit `e68aa0a`, 2026-06-18

## Why this matters

Plan 014 added `GET /media?u=<url>` (`src/routes.rs`), a passthrough that streams
Facebook CDN media through facebed. It guards the **initial** URL host with
`media_target_allowed` (allowlist via `is_facebook_media_host`), then fetches with
the **shared** `reqwest` client — which sets **no redirect policy** and therefore
follows up to 10 redirects by default (`src/fetch.rs:131-136`).

The allowlist accepts **any** `*.facebook.com` host, including Facebook's
open-redirect endpoints (e.g. `l.facebook.com/l.php?u=…`, `lm.facebook.com`).
So this request passes the guard and then gets followed to an arbitrary internal
target, whose body is streamed back to the caller:

```
GET /media?u=https://l.facebook.com/l.php?u=http://169.254.169.254/latest/meta-data
```

That is a full **read-SSRF**: cloud metadata, internal admin endpoints, anything
the server can reach. The route is **live and directly reachable**
(`src/routes.rs` registers `/media`) even though embeds don't link to it yet. The
014 spike outcome explicitly flagged "redirect-hop host validation" as a required
follow-up before `/media` is trusted.

This plan gives `/media` a **dedicated** HTTP client whose redirect policy
re-validates **every** hop against the same Facebook-media allowlist and caps the
hop count. The shared client used by the normal fetch path is left alone (it must
keep following redirects — login/checkpoint detection in `fetch.rs` depends on
it).

## Current state

- `src/routes.rs` — the `/media` handler and its guard (line numbers from
  `e68aa0a`):
  ```rust
  // src/routes.rs:143-180 (the handler body)
  let Ok(parsed) = Url::parse(&p.u) else {
      return (StatusCode::BAD_REQUEST, "bad url").into_response();
  };
  if !matches!(parsed.scheme(), "http" | "https") {
      return (StatusCode::BAD_REQUEST, "bad scheme").into_response();
  }
  if !media_target_allowed(&p.u) {
      return (StatusCode::FORBIDDEN, "host not allowed").into_response();
  }

  let upstream = match state.fetcher.client().get(parsed).send().await {   // <-- shared client, follows redirects
      Ok(r) => r,
      Err(_) => return (StatusCode::BAD_GATEWAY, "upstream error").into_response(),
  };
  // ... content-length cap, content-type passthrough, stream body ...
  ```
  ```rust
  // src/routes.rs:182-194 — the initial-host guard (KEEP; it's correct for hop 0)
  fn media_target_allowed(u: &str) -> bool {
      match Url::parse(u) {
          Ok(parsed) => {
              matches!(parsed.scheme(), "http" | "https")
                  && parsed.host_str()
                      .map(crate::url_clean::is_facebook_media_host)
                      .unwrap_or(false)
          }
          Err(_) => false,
      }
  }
  ```
- `src/fetch.rs` — the shared client builder. **Do not change its redirect
  behavior**; add a *second* client next to it:
  ```rust
  // src/fetch.rs:130-142
  pub fn new(cookies: Arc<arc_swap::ArcSwap<CookieJar>>) -> anyhow::Result<Self> {
      let client = Client::builder()
          .gzip(true)
          .brotli(true)
          .connect_timeout(Duration::from_secs(5))
          .timeout(Duration::from_secs(8))
          .build()?;
      Ok(Self {
          client,
          cookies,
          media_size_cache: Mutex::default(),
      })
  }

  pub fn client(&self) -> &Client {   // src/fetch.rs:144
      &self.client
  }
  ```
  The `Fetcher` struct is declared at `src/fetch.rs:22` — you'll add one field.
  `Client` and `Duration` are already imported in `fetch.rs`.
- `src/url_clean.rs:87` — the allowlist predicate, already public, reused for
  every hop:
  ```rust
  pub fn is_facebook_media_host(host: &str) -> bool {
      let host = host.to_ascii_lowercase();
      is_facebook_page_host(&host) || host == "fbcdn.net" || host.ends_with(".fbcdn.net")
  }
  ```
- **`reqwest::redirect`** is available (reqwest is a dependency). The custom-policy
  API is `reqwest::redirect::Policy::custom(|attempt| { ... attempt.follow() /
  attempt.stop() / attempt.error(..) })`; `attempt.url()` is the **next** URL,
  `attempt.previous()` is the chain so far (use its length to cap hops).
- **Convention**: shared services hang off `Fetcher`, built once in `Fetcher::new`
  (the existing `client` is the exemplar). Pure, testable helpers live as free
  functions with a `#[cfg(test)]` test nearby — see `media_target_allowed` and its
  test `media_guard_blocks_non_facebook_hosts` in `src/routes.rs`.

## Commands you will need

| Purpose   | Command                              | Expected on success       |
|-----------|--------------------------------------|---------------------------|
| Build     | `cargo build`                        | exit 0                    |
| Tests     | `cargo test`                         | all pass (92 today + new) |
| One test  | `cargo test media_redirect`          | the new test(s) pass      |
| Format    | `cargo fmt -- --check`               | exit 0, no diff           |

## Scope

**In scope** (the only files you should modify):
- `src/fetch.rs` — add a `media_client` field, build it with a hop-revalidating
  redirect policy in `Fetcher::new`, expose `media_client()`. Add the pure
  hop-decision helper + its unit test.
- `src/routes.rs` — change the `/media` handler to fetch with
  `state.fetcher.media_client()` instead of `.client()`.
- `plans/README.md` — status row + reconcile note.

**Out of scope** (do NOT touch):
- The shared `client` in `fetch.rs` and every caller of `.client()` — the normal
  fetch path **must** keep following redirects (login/checkpoint detection relies
  on landing on the redirected page). Changing it would break `fetch.rs`'s
  page-type probing.
- `media_target_allowed` and the rest of the `/media` handler logic (scheme check,
  content-length cap, streaming) — leave them; this plan only changes which client
  does the fetch.
- `src/url_clean.rs` — reuse `is_facebook_media_host` as-is.
- `Cargo.toml` — no new dependency or feature needed.

## Git workflow

- Branch: you are likely on `advisor/014-media-proxy` (where 014 landed). Either
  continue there or branch `advisor/015-media-ssrf` from it — match what the
  operator wants; if unsure, stay on the current branch.
- Commit style: short imperative subject, no Conventional Commits prefix (e.g.
  "Block redirect-hop SSRF on /media").
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add the hop-decision helper (pure, testable)

In `src/fetch.rs`, add a small free function and a hop cap constant:

```rust
/// Max redirect hops the media proxy will follow.
const MEDIA_MAX_REDIRECTS: usize = 4;

/// Decide whether the media proxy may follow a redirect to `next_host` after
/// `hops_so_far` hops. Every hop must stay on the Facebook media allowlist, and
/// the chain is capped. `next_host` is `None` when the URL has no host.
pub fn media_redirect_ok(next_host: Option<&str>, hops_so_far: usize) -> bool {
    hops_so_far < MEDIA_MAX_REDIRECTS
        && next_host
            .map(crate::url_clean::is_facebook_media_host)
            .unwrap_or(false)
}
```

### Step 2: Build the dedicated media client in `Fetcher::new`

Add a `media_client: Client` field to the `Fetcher` struct (`src/fetch.rs:22`),
and build it in `new` with a custom redirect policy that calls `media_redirect_ok`
on every hop:

```rust
let media_client = Client::builder()
    .gzip(true)
    .brotli(true)
    .connect_timeout(Duration::from_secs(5))
    .timeout(Duration::from_secs(8))
    .redirect(reqwest::redirect::Policy::custom(|attempt| {
        let host = attempt.url().host_str().map(|h| h.to_owned());
        if media_redirect_ok(host.as_deref(), attempt.previous().len()) {
            attempt.follow()
        } else {
            attempt.stop()
        }
    }))
    .build()?;
```

Add `media_client` to the returned `Self { … }`, and a getter:

```rust
pub fn media_client(&self) -> &Client {
    &self.media_client
}
```

(`attempt.stop()` ends the chain and returns the redirect response itself — the
handler in Step 3 then sees a non-success 3xx status and returns `502`, so a
blocked redirect never reaches the disallowed host. This is the desired
fail-closed behavior.)

**Verify**: `cargo build` → exit 0.

### Step 3: Point the `/media` handler at the dedicated client

In `src/routes.rs`, in the `media` handler, change the one fetch line:

```rust
// before:
let upstream = match state.fetcher.client().get(parsed).send().await {
// after:
let upstream = match state.fetcher.media_client().get(parsed).send().await {
```

Nothing else in the handler changes.

**Verify**: `cargo build` → exit 0; `cargo fmt -- --check` → exit 0.

### Step 4: Unit-test the hop decision (the security-critical logic)

In `src/fetch.rs`'s test module (there is already a `#[cfg(test)] mod tests` in
this file — add to it; if not, create one), test `media_redirect_ok`:

```rust
#[test]
fn media_redirect_blocks_offsite_and_caps_hops() {
    use super::media_redirect_ok;
    // allowed hosts, within hop budget → follow
    assert!(media_redirect_ok(Some("scontent.xx.fbcdn.net"), 0));
    assert!(media_redirect_ok(Some("video.fbcdn.net"), 2));
    // the SSRF payload target → never followed
    assert!(!media_redirect_ok(Some("169.254.169.254"), 0));
    assert!(!media_redirect_ok(Some("evil.example.com"), 0));
    assert!(!media_redirect_ok(Some("evilfbcdn.net"), 0)); // lookalike
    // host-less (e.g. file:) → blocked
    assert!(!media_redirect_ok(None, 0));
    // hop cap: even an allowed host is refused past the budget
    assert!(!media_redirect_ok(Some("scontent.xx.fbcdn.net"), 4));
}
```

**Verify**: `cargo test media_redirect` → passes. `cargo test` → all pass.

## Test plan

- New test `media_redirect_blocks_offsite_and_caps_hops` in `src/fetch.rs` — the
  load-bearing cases are the `169.254.169.254`, `evilfbcdn.net`, and hop-cap
  assertions (they prove the SSRF chain can't escape the allowlist). Model after
  the existing `media_guard_blocks_non_facebook_hosts` test in `src/routes.rs`.
- Manual (optional, not a `cargo` gate): start the server and confirm
  `curl -s "localhost:9812/media?u=https://l.facebook.com/l.php?u=http://169.254.169.254/"`
  returns `502`/`403`, **not** any internal content; and that a direct
  `scontent.*.fbcdn.net` image still returns `200`.

## Done criteria

ALL must hold:

- [ ] `cargo build` exits 0
- [ ] `cargo fmt -- --check` exits 0 (no diff)
- [ ] `cargo test` exits 0; `media_redirect_blocks_offsite_and_caps_hops` passes,
      including the `169.254.169.254`, `evilfbcdn.net`, and hop-cap cases
- [ ] `grep -n "media_client()" src/routes.rs` shows the `/media` handler using it
- [ ] `grep -n "Policy::custom\|media_redirect_ok" src/fetch.rs` shows the policy
      wired to the helper
- [ ] The shared `client` builder (`src/fetch.rs:131-136`) is **unchanged** — no
      `.redirect(...)` added to it (`git diff` shows the only client-builder change
      is the new `media_client`)
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report (do not improvise) if:

- The drift check shows `src/routes.rs`/`src/fetch.rs` changed and excerpts no
  longer match.
- The reqwest version in `Cargo.lock` lacks `reqwest::redirect::Policy::custom` /
  `attempt.previous()` (it shouldn't — reqwest 0.12 has them); report the API
  mismatch rather than reaching for a different mechanism.
- Implementing the policy would require changing the shared `client` or any
  `.client()` caller — that's out of scope; report it.
- You cannot make `media_redirect_ok` a pure free function (e.g. a borrow issue in
  the closure) — report it rather than inlining untestable logic into the closure.

## Maintenance notes

For whoever owns this next:

- **Why a second client and not just `Policy::none()`**: some legitimate FB media
  URLs 302 between `fbcdn` hosts; `Policy::none()` would break those. The custom
  policy follows *allowlisted* hops only, so legit `fbcdn→fbcdn` redirects still
  work while `facebook.com→internal` is refused. If you ever see legit media
  failing with `502`, check whether a real hop host falls outside
  `is_facebook_media_host` before loosening anything.
- **Fail-closed**: a blocked redirect returns the 3xx to the handler, which maps
  non-success to `502`. Verify any future handler refactor preserves the
  "non-2xx ⇒ error, never stream" branch.
- **Still deferred from the 014 spike** (not this plan): Range/`206` support, a
  hard *streaming* byte cap (the current `Content-Length` cap misses chunked
  upstreams), and a per-`/media` rate/concurrency budget. Those are durability/
  abuse concerns, separate from this SSRF fix.
- **Reviewer**: treat `media_redirect_ok` and the policy closure as
  security-critical. Confirm the hop cap and the allowlist call are both present,
  and that the shared client was not touched.
