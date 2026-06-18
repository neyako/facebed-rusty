# Plan 004: Enforce a Facebook-host boundary on cookie-bearing fetches and the human redirect

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat 3b6521c..HEAD -- src/url_clean.rs src/fetch.rs src/routes.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts below against the live code before editing; on a mismatch,
> treat it as a STOP condition.

## Status

- **Priority**: P0 (security — fix first)
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `3b6521c`, 2026-06-17

## Why this matters

facebed only ever intends to fetch Facebook and to redirect humans to Facebook,
but two code paths trust a request path that is *already* an absolute URL and do
not constrain its host:

1. **Cookie exfiltration via the content fetch.** `ensure_absolute` returns an
   input that starts with `http://`/`https://` unchanged. The `?type=3`
   pre-dispatch branch in `routes.rs` passes the **raw** request path to the
   parser before host normalization, and the parser calls
   `fetcher.fetch(path, true)`, which attaches the configured Facebook account
   cookie header with no host check. A crafted absolute-URL path therefore makes
   the server send its Facebook session cookies (`c_user`, `xs`, …) to an
   arbitrary host. Those cookies are long-lived account credentials; leaking them
   is full account compromise. Treat this as the priority fix.
2. **Open redirect.** For non-crawler user agents the handler responds `301` with
   `Location` set to `ensure_absolute(path)`. A crafted absolute-URL path makes
   the trusted facebed domain redirect to an attacker site — a phishing primitive.

The normal dispatch path is already safe because `clean_path` strips the host to
a Facebook-relative path before the parser runs; only the `?type=3` shortcut and
the redirect bypass that. This plan adds a single host-allowlist boundary at the
sinks: the cookie-bearing fetch refuses non-Facebook hosts (so cookies can never
leave Facebook), the video-size probe refuses non-Facebook/CDN hosts (blind SSRF
hardening), the redirect only points at Facebook, and the `?type=3` branch
normalizes its path like the main flow does.

## Current state

Three files change.

### `src/url_clean.rs` — `ensure_absolute` (lines 71-77)

```rust
pub fn ensure_absolute(input: &str) -> String {
    if input.starts_with("http://") || input.starts_with("https://") {
        input.to_owned()
    } else {
        format!("{}/{}", FB_BASE, input.trim_start_matches('/'))
    }
}
```

This file already `use url::Url;` (line 3) and has a `#[cfg(test)] mod tests`
(line 101). There is an existing host-parsing helper in `src/fetch.rs`,
`facebook_path_from_url` (fetch.rs:695-712), which matches exactly
`www.facebook.com | facebook.com | m.facebook.com` — useful as a reference for
which hosts count as Facebook, but do not reuse it directly (it strips to a path;
we need a boolean host check).

### `src/routes.rs` — `?type=3` pre-dispatch (lines 168-178) and crawler-gate redirect (lines 180-194)

```rust
    // image-in-comment priority
    if let Ok(parsed) = Url::parse(&format!("https://www.facebook.com/{path}")) {
        let types: Vec<String> = parsed
            .query_pairs()
            .filter(|(k, _)| k == "type")
            .map(|(_, v)| v.into_owned())
            .collect();
        if types.iter().any(|t| t.contains('3')) {
            return process(&state, &path, ParserKind::Photocom).await;
        }
    }

    // crawler gate
    if !is_bot {
        let target = url_clean::ensure_absolute(&path);
        let body = format_redirect_page(&target);
        let mut hdrs = HeaderMap::new();
        hdrs.insert(
            axum::http::header::LOCATION,
            HeaderValue::from_str(&target).unwrap_or_else(|_| HeaderValue::from_static("/")),
        );
        hdrs.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        return (StatusCode::MOVED_PERMANENTLY, hdrs, body).into_response();
    }
```

`process(&state, &path, ParserKind::Photocom)` runs the parser, which calls
`ctx.fetcher.fetch(post_path, true)` — `path` here is the raw request path,
**not** run through `clean_path`. Confirm the parser's fetch call:
`grep -n "fetch(post_path" src/parsers/photocom.rs` → one match (line 13).

### `src/fetch.rs` — `fetch` (lines 318-332), `fetch_until` head, `head_content_length` (lines 255-316), `request_for` (lines 371-393)

```rust
    pub async fn fetch(&self, post_path: &str, use_cookies: bool) -> FacebedResult<FetchedPage> {
        let started = Instant::now();
        let url = ensure_absolute(post_path);
        let (req, account_label) = self.request_for(&url, use_cookies);
        ...
```

`fetch_until` similarly begins `let url = ensure_absolute(post_path);` (after
plan 001 it may have additional lines, but that statement is unchanged — if
plan 001 has not landed, this plan still applies, just confirm the line is there).

`request_for` (the cookie attachment):

```rust
    fn request_for(&self, url: &str, use_cookies: bool) -> (RequestBuilder, String) {
        let mut req = self.client.get(url);
        ...
        if use_cookies {
            ...
            req = req.header("cookie", acc.header_value());
            ...
        }
        (req.header("user-agent", user_agent), account_label)
    }
```

`head_content_length` begins:

```rust
    pub async fn head_content_length(&self, url: &str) -> Option<u64> {
        let started = Instant::now();
        let now = Instant::now();
        if let Ok(mut cache) = self.media_size_cache.lock() {
            ...
        }
        let resp = match self.client.head(url).timeout(VIDEO_HEAD_TIMEOUT).send().await {
            ...
```

The `url` here is a video URL pulled from Facebook JSON (`progressive_url`,
`playable_url`, …); video URLs live on `*.fbcdn.net` / `*.facebook.com`.

`src/fetch.rs` already `use url::Url;` (line 13) and has a `#[cfg(test)] mod
tests` (line 944).

### Repo conventions to follow

- Errors are a single `FacebedError` enum; construct via associated fns
  (`FacebedError::no_data(...)`). The fetch guard returns `NoData` so a refused
  host renders the standard "C" embed (no new error letter — `AGENTS.md` forbids
  changing the `C/P/U/X/T` set).
- Host checks live in `url_clean.rs` (path/URL hygiene already lives there).
- Tests go in the `#[cfg(test)] mod tests` at the bottom of each file. `url_clean.rs`
  uses plain `assert!`/`assert_eq!`; follow it.
- Hard gates: `cargo fmt --check` and `cargo test`.
- Security framing: this is defensive hardening. Do not add exploit strings or
  attack walkthroughs to code, comments, or commits — comments should state the
  invariant ("only Facebook hosts may receive the account cookie"), not how to
  break it.

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
- `src/url_clean.rs` — add host-allowlist helpers + tests.
- `src/fetch.rs` — guard `fetch`/`fetch_until` (cookie boundary) and
  `head_content_length` (media boundary); add tests.
- `src/routes.rs` — normalize the `?type=3` path; force a Facebook host on the
  redirect target.

**Out of scope** (do NOT touch):
- `resolve_share_link*` (fetch.rs) — already constrained: the share regexes
  (`RE_SHARE_V`/`RE_SHARE_PR`, routes.rs) only match paths starting with
  `share/`, so those fetchers receive Facebook-relative paths. Adding the guard
  there is sensible defense-in-depth but is a separate follow-up; leave it.
- `ensure_absolute` itself — it is also used to build *display* URLs from
  Facebook-absolute values (e.g. `story.url`), so it must keep accepting absolute
  Facebook URLs. Do the host enforcement at the sinks, not by changing
  `ensure_absolute`.
- The error-letter set.

## Git workflow

- Branch: `advisor/004-host-boundary`.
- Commit style: short imperative subject (e.g. "Refuse non-Facebook hosts on
  cookie fetch and redirect").
- Do NOT push or open a PR unless the operator instructs it.

## Steps

### Step 1: Add host-allowlist helpers in `src/url_clean.rs`

Add (e.g. just above the `#[cfg(test)] mod tests`):

```rust
/// True for Facebook page hosts. Matches `facebook.com` and any subdomain
/// (`www.`, `m.`, `web.`, `mbasic.`, …) but NOT lookalikes such as
/// `evilfacebook.com` or `facebook.com.evil.com`.
pub fn is_facebook_page_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "facebook.com" || host.ends_with(".facebook.com")
}

/// True for hosts that may serve Facebook media (page hosts plus the
/// `fbcdn.net` CDN that video/image URLs live on).
pub fn is_facebook_media_host(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    is_facebook_page_host(&lower) || lower == "fbcdn.net" || lower.ends_with(".fbcdn.net")
}

/// True iff `url` parses, is absolute, and its host is a Facebook page host.
pub fn is_facebook_page_url(url: &str) -> bool {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(is_facebook_page_host))
        .unwrap_or(false)
}
```

Add tests to the `url_clean.rs` `mod tests`:

```rust
    #[test]
    fn host_allowlist_accepts_facebook_rejects_lookalikes() {
        assert!(is_facebook_page_host("www.facebook.com"));
        assert!(is_facebook_page_host("m.facebook.com"));
        assert!(is_facebook_page_host("facebook.com"));
        assert!(!is_facebook_page_host("evilfacebook.com"));
        assert!(!is_facebook_page_host("facebook.com.evil.com"));
        assert!(!is_facebook_page_host("example.com"));

        assert!(is_facebook_media_host("scontent.xx.fbcdn.net"));
        assert!(!is_facebook_media_host("example.com"));

        assert!(is_facebook_page_url("https://www.facebook.com/groups/1/posts/2"));
        assert!(!is_facebook_page_url("https://example.com/x"));
        assert!(!is_facebook_page_url("not a url"));
    }
```

**Verify**: `cargo test --locked url_clean` → all pass including the new test.

### Step 2: Guard the cookie-bearing fetch in `src/fetch.rs`

Add a helper near the other free functions:

```rust
/// Resolve `post_path` to an absolute URL, refusing anything that is not a
/// Facebook page host. Security boundary: content fetches attach the account
/// cookie, so a non-Facebook host here would leak the session cookie. Returns
/// `NoData` (renders the standard "C" embed) for a refused host.
fn facebook_fetch_url(post_path: &str) -> FacebedResult<String> {
    let url = ensure_absolute(post_path);
    let allowed = Url::parse(&url)
        .ok()
        .and_then(|u| u.host_str().map(crate::url_clean::is_facebook_page_host))
        .unwrap_or(false);
    if allowed {
        Ok(url)
    } else {
        Err(FacebedError::no_data(format!(
            "refusing to fetch non-Facebook host for {post_path}"
        )))
    }
}
```

In `fetch`, replace `let url = ensure_absolute(post_path);` with:

```rust
        let url = facebook_fetch_url(post_path)?;
```

In `fetch_until`, replace its `let url = ensure_absolute(post_path);` with the
same line. (Both functions currently start the body with that `ensure_absolute`
statement; this is the only change to each.)

Add a test to the `fetch.rs` `mod tests`:

```rust
    #[test]
    fn fetch_url_guard_allows_facebook_refuses_other_hosts() {
        use crate::error::FacebedError;
        // Relative path → forced onto facebook.com.
        assert_eq!(
            super::facebook_fetch_url("groups/1/posts/2").unwrap(),
            "https://www.facebook.com/groups/1/posts/2"
        );
        // Absolute Facebook URL is allowed.
        assert!(super::facebook_fetch_url("https://m.facebook.com/x").is_ok());
        // Absolute non-Facebook URL is refused before any cookie can be attached.
        assert!(matches!(
            super::facebook_fetch_url("https://example.com/x?type=3"),
            Err(FacebedError::NoData(_))
        ));
    }
```

**Verify**: `cargo build` → exit 0. `cargo test --locked fetch` → all pass
including the new test.

### Step 3: Guard the video-size probe in `src/fetch.rs`

At the very top of `head_content_length`, before the cache lookup, refuse
non-media hosts (returning `None` is the existing "size unknown → render inline"
default, so behavior for legitimate hosts is unchanged):

```rust
    pub async fn head_content_length(&self, url: &str) -> Option<u64> {
        let host_ok = Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(crate::url_clean::is_facebook_media_host))
            .unwrap_or(false);
        if !host_ok {
            return None;
        }
        let started = Instant::now();
        ...
```

**Verify**: `cargo build` → exit 0. `cargo test --locked fetch` → still all pass.

### Step 4: Normalize the `?type=3` path and harden the redirect in `src/routes.rs`

(a) In the `?type=3` branch, normalize the path the same way the main flow does
(`clean_path` strips the spoofable host while preserving the `type=3` query),
before handing it to the parser:

```rust
        if types.iter().any(|t| t.contains('3')) {
            let cleaned = url_clean::clean_path(&path);
            return process(&state, &cleaned, ParserKind::Photocom).await;
        }
```

(b) In the crawler-gate redirect, force the target onto a Facebook host:

```rust
    if !is_bot {
        let target = url_clean::ensure_absolute(&path);
        let target = if url_clean::is_facebook_page_url(&target) {
            target
        } else {
            String::from("https://www.facebook.com/")
        };
        let body = format_redirect_page(&target);
        ...
```

(Leave the rest of the redirect block — header inserts, status — unchanged; just
feed it the host-checked `target`.)

**Verify**: `cargo build` → exit 0. `cargo test --locked` → entire suite passes.

### Step 5: Format

Run `cargo fmt`, then confirm clean.

**Verify**: `cargo fmt --check` → exit 0.

## Test plan

- `src/url_clean.rs`: `host_allowlist_accepts_facebook_rejects_lookalikes` — the
  allowlist accepts `facebook.com`/subdomains and `*.fbcdn.net`, and rejects
  `example.com`, `evilfacebook.com`, and `facebook.com.evil.com` (the suffix
  bypass). This is the core security assertion.
- `src/fetch.rs`: `fetch_url_guard_allows_facebook_refuses_other_hosts` — a
  relative path resolves to facebook.com; an absolute Facebook URL is allowed; an
  absolute non-Facebook URL is refused with `NoData` *before* `request_for`
  attaches a cookie.
- Structural pattern: existing `mod tests` in `url_clean.rs` and `fetch.rs`.
- Verification: `cargo test --locked` → all pass.
- Manual (optional, document the result, do not paste exploit traffic): with the
  server running, a crawler-UA request whose path is an absolute non-Facebook URL
  should return the standard error embed, and the access log of that external
  host should show no request from facebed.

## Done criteria

ALL must hold:

- [ ] `cargo build` exits 0.
- [ ] `cargo test --locked` exits 0; the two new tests pass; no pre-existing test regressed.
- [ ] `cargo fmt --check` exits 0.
- [ ] `grep -n "ensure_absolute(post_path)" src/fetch.rs` returns no matches in
      `fetch`/`fetch_until` (both now use `facebook_fetch_url`); confirm with
      `grep -n "facebook_fetch_url" src/fetch.rs` → at least two call sites.
- [ ] `grep -n "is_facebook_media_host" src/fetch.rs` → used in `head_content_length`.
- [ ] `grep -n "is_facebook_page_url" src/routes.rs` → used in the redirect.
- [ ] `grep -n "process(&state, &path, ParserKind::Photocom)" src/routes.rs`
      returns no matches (the `?type=3` branch now passes the cleaned path).
- [ ] `git status` shows only `src/url_clean.rs`, `src/fetch.rs`, `src/routes.rs` modified.
- [ ] `plans/README.md` status row for 004 updated.

## STOP conditions

Stop and report back (do not improvise) if:

- The drift check shows any in-scope file changed since `3b6521c` and the
  "Current state" excerpts no longer match.
- After the guard, any legitimate Facebook fetch path is refused — e.g. a parser
  test that fetches a relative Facebook path starts failing. That means
  `is_facebook_page_host` is too strict; report it rather than widening the
  allowlist to non-Facebook hosts.
- You find an additional code path that calls `fetcher.fetch(..., true)` or
  `client.get/head` with cookies on a path that has NOT been host-checked, beyond
  the ones listed here — report it; the boundary may be incomplete.
- Normalizing the `?type=3` path with `clean_path` drops the `type=3` query and
  breaks the photocom parser (it should not — `type` is not in `DROP_KEYS`); if it
  does, report it instead of special-casing.

## Maintenance notes

- The security invariant is "the account cookie is only ever sent to a Facebook
  page host." Any new fetch path that attaches cookies must route through
  `facebook_fetch_url` (or an equivalent host check). Reviewers should treat a new
  `client.get/head` with a cookie header as requiring a host guard.
- Deferred (intentional): `resolve_share_link_body`/`_head` are not guarded here
  because the `share/` dispatch regexes already keep their input Facebook-relative.
  If share dispatch is ever broadened, add the same `is_facebook_page_host` guard
  there.
- `head_content_length` returning `None` for a refused host means "render the
  video inline" (the existing default). That is the safe choice — a non-Facebook
  media host should not have been produced by a real Facebook post anyway, and we
  do not want to block legitimate embeds.
- If a future feature legitimately needs to fetch a non-Facebook host (e.g.
  resolving external link-card targets), do it on a **separate, cookie-less**
  client path — never the cookie-bearing one.
