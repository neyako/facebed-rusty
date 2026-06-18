# Plan 001: Make the `fetch_until` partial-fetch path linear instead of O(N²)

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat 3b6521c..HEAD -- src/fetch.rs src/parsers/json_post.rs`
> If either file changed since this plan was written, compare the "Current
> state" excerpts below against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: none (but see Maintenance notes: do this BEFORE plan 003 — both edit `fetch_until`)
- **Category**: perf
- **Planned at**: commit `3b6521c`, 2026-06-17

## Why this matters

`Fetcher::fetch_until` is the "stop downloading once we have the post block"
fast path used for the most common Facebook post type (numeric group/page
posts). It is supposed to make crawls faster. Today it makes the CPU side
*slower the bigger the page gets*: on every streamed chunk it re-decodes the
entire accumulated body with `String::from_utf8_lossy(&body)` and then its stop
predicate (`has_completed_matching_post_block`) re-scans every `<script>` tag
from byte 0. Both are O(N) per chunk, so over a body that arrives in K chunks
the work is O(K·N) ≈ O(N²), plus a full re-allocation on every chunk that ends
mid-multibyte-character (common, since Facebook HTML is UTF-8 and chunk
boundaries are arbitrary). After this plan, the same early-stop behavior runs in
a single linear pass with no per-chunk full-buffer decode. The network saving
(stopping the download early) is unchanged; only the wasted CPU/alloc is removed.

## Current state

Files:

- `src/fetch.rs` — `Fetcher::fetch_until` (the streaming loop). The only caller
  is `src/parsers/json_post.rs`.
- `src/parsers/json_post.rs` — `JsonPostParser::process` builds the stop
  predicate `has_completed_matching_post_block`.

### `src/fetch.rs` — current `fetch_until` (lines 337-369)

```rust
    pub async fn fetch_until<F>(
        &self,
        post_path: &str,
        use_cookies: bool,
        should_stop: F,
    ) -> FacebedResult<FetchedPage>
    where
        F: FnMut(&str) -> bool,
    {
        let mut should_stop = should_stop;
        let started = Instant::now();
        let url = ensure_absolute(post_path);
        let (req, account_label) = self.request_for(&url, use_cookies);
        let mut resp = req.send().await?;
        let response_ms = started.elapsed().as_millis();
        let status = resp.status();
        let final_url = resp.url().to_string();
        let mut body = Vec::new();
        let mut stopped_early = false;
        let read_started = Instant::now();
        while let Some(chunk) = resp.chunk().await? {
            body.extend_from_slice(&chunk);
            let html = String::from_utf8_lossy(&body);
            if should_stop(&html) {
                stopped_early = true;
                break;
            }
        }
        let read_ms = read_started.elapsed().as_millis();
        let html = String::from_utf8_lossy(&body).into_owned();
        tracing::info!(path = %post_path, account = %account_label, status = %status, final_url = %final_url, len = html.len(), partial = stopped_early, response_ms, read_ms, total_ms = started.elapsed().as_millis(), "fetch done");
        self.page_from_html(url, html, post_path, stopped_early)
    }
```

The two cost centers: `let html = String::from_utf8_lossy(&body);` (decodes/validates
the whole buffer every chunk) and `should_stop(&html)` (the predicate below, which
rescans from the start every call).

### `src/parsers/json_post.rs` — call site (lines 17-24) and predicate (lines 156-181)

```rust
        if should_try_partial_fetch(post_id.as_deref()) {
            let pid = post_id.clone().unwrap_or_default();
            let page = ctx
                .fetcher
                .fetch_until(post_path, true, |html| {
                    has_completed_matching_post_block(html, &pid)
                })
                .await?;
```

```rust
fn has_completed_matching_post_block(html: &str, post_id: &str) -> bool {
    let mut offset = 0;
    while let Some(script_rel) = html[offset..].find("<script") {
        let start = offset + script_rel;
        let Some(open_end_rel) = html[start..].find('>') else {
            return false;
        };
        let open_end = start + open_end_rel + 1;
        let open_tag = &html[start..open_end];
        let Some(close_rel) = html[open_end..].find("</script>") else {
            return false;
        };
        let close_start = open_end + close_rel;
        if open_tag.contains(r#"type="application/json""#)
            && open_tag.contains("data-content-len")
            && open_tag.contains("data-sjs")
        {
            let body = &html[open_end..close_start];
            if body.contains("i18n_reaction_count") && body.contains(post_id) {
                return true;
            }
        }
        offset = close_start + "</script>".len();
    }
    false
}
```

Existing unit test for the predicate (json_post.rs lines 314-330), which this
plan will update:

```rust
    #[test]
    fn matching_post_block_requires_completed_script() {
        let open = r#"<script type="application/json" data-content-len="42" data-sjs>"#;
        let body = r#"{"i18n_reaction_count":"1K","id":"123"}"#;
        assert!(!has_completed_matching_post_block(
            &format!("{open}{body}"),
            "123"
        ));
        assert!(has_completed_matching_post_block(
            &format!("{open}{body}</script><div>later</div>"),
            "123"
        ));
        assert!(!has_completed_matching_post_block(
            &format!("{open}{body}</script>"),
            "456"
        ));
    }
```

### Key facts the design relies on

- `should_try_partial_fetch` (json_post.rs:149-154) only enables this path when
  `post_id` is **all ASCII digits**. So `post_id` is always ASCII here, and a
  byte-substring search for it is safe.
- All the markers searched for (`<script`, `>`, `</script>`,
  `type="application/json"`, `data-content-len`, `data-sjs`,
  `i18n_reaction_count`) are pure ASCII.
- `fetch_until` has exactly one caller (`json_post.rs`). Changing its closure
  signature is therefore a one-call-site change. Confirm with:
  `grep -rn "fetch_until" src` → should list only `src/fetch.rs` (definition)
  and `src/parsers/json_post.rs` (call).

### Repo conventions to follow

- Error handling: functions return `FacebedResult<T>` (`= Result<T, FacebedError>`);
  `fetch_until` already does — keep it.
- Tests live in a `#[cfg(test)] mod tests` at the bottom of the same file
  (see `src/parsers/json_post.rs:253` and `src/fetch.rs:944`). Add new tests there.
- No CI-enforced clippy; the hard gates are `cargo fmt --check` and `cargo test`.
- Match the existing terse helper-function style in `json_post.rs`.

## Commands you will need

| Purpose      | Command                 | Expected on success      |
|--------------|-------------------------|--------------------------|
| Build        | `cargo build`           | exit 0, no errors        |
| Tests        | `cargo test --locked`   | all pass (no failures)   |
| Format       | `cargo fmt`             | reformats in place       |
| Format check | `cargo fmt --check`     | exit 0 (no diff)         |

(These are the exact gates the CI runs: see `.github/workflows/docker-build.yml`
→ `cargo fmt --check` and `cargo test --locked`.)

## Scope

**In scope** (the only files you may modify):
- `src/fetch.rs` — change `fetch_until`'s closure type and loop.
- `src/parsers/json_post.rs` — replace the predicate with a stateful, byte-based
  scanner; update its unit test.

**Out of scope** (do NOT touch):
- `Fetcher::fetch` (the full-read path) — unrelated.
- `page_from_html`, `get_json_blocks`, `get_json_block_texts` — the parsing that
  happens after the body is read is unchanged.
- The partial-parse fallback logic in `JsonPostParser::process` (json_post.rs:25-35)
  — leave the `match parse_page(...) { Ok => ..., Err(e) if page.is_partial() => retry full, ... }`
  exactly as-is. It is the safety net for this change.
- Any other parser or any embed-format code.

## Git workflow

- Branch: `advisor/001-fetch-until-linear` (or the repo's convention if one is evident).
- Commit style: short imperative subject, no Conventional-Commits prefix (match
  `git log --oneline`, e.g. "Make partial fetch scan linear").
- Do NOT push or open a PR unless the operator instructs it.

## Steps

### Step 1: Change `fetch_until` to a byte-based, single-decode loop

In `src/fetch.rs`, replace the `fetch_until` function (lines 337-369) with the
version below. The two changes: the closure now takes `&[u8]` (so we never
decode the whole buffer mid-stream), and the per-chunk `String::from_utf8_lossy`
inside the loop is removed (we decode exactly once, after the loop).

```rust
    pub async fn fetch_until<F>(
        &self,
        post_path: &str,
        use_cookies: bool,
        mut should_stop: F,
    ) -> FacebedResult<FetchedPage>
    where
        F: FnMut(&[u8]) -> bool,
    {
        let started = Instant::now();
        let url = ensure_absolute(post_path);
        let (req, account_label) = self.request_for(&url, use_cookies);
        let mut resp = req.send().await?;
        let response_ms = started.elapsed().as_millis();
        let status = resp.status();
        let final_url = resp.url().to_string();
        let mut body = Vec::new();
        let mut stopped_early = false;
        let read_started = Instant::now();
        while let Some(chunk) = resp.chunk().await? {
            body.extend_from_slice(&chunk);
            if should_stop(&body) {
                stopped_early = true;
                break;
            }
        }
        let read_ms = read_started.elapsed().as_millis();
        let html = String::from_utf8_lossy(&body).into_owned();
        tracing::info!(path = %post_path, account = %account_label, status = %status, final_url = %final_url, len = html.len(), partial = stopped_early, response_ms, read_ms, total_ms = started.elapsed().as_millis(), "fetch done");
        self.page_from_html(url, html, post_path, stopped_early)
    }
```

**Verify**: `cargo build` → fails to compile *only* in `src/parsers/json_post.rs`
(the call site still passes a `&str` closure). That is expected; Step 2 fixes it.
`cargo build 2>&1 | grep -c "src/fetch.rs"` → `0` (no errors originate in fetch.rs).

### Step 2: Replace the predicate with a stateful byte scanner in `json_post.rs`

In `src/parsers/json_post.rs`:

(a) Replace the call site (lines 19-24) so the closure owns its incremental
scan state across chunks:

```rust
            let pid = post_id.clone().unwrap_or_default();
            let mut scanner = PostBlockScanner::default();
            let page = ctx
                .fetcher
                .fetch_until(post_path, true, |bytes| scanner.found_match(bytes, &pid))
                .await?;
```

(b) Delete the old `has_completed_matching_post_block` function (lines 156-181)
and add, in its place, the byte scanner plus two tiny search helpers:

```rust
/// Incremental, forward-only scanner for the partial-fetch stop condition.
/// Returns `true` once a *completed* `<script type="application/json"
/// data-content-len ... data-sjs>...</script>` block has been seen whose body
/// contains both `i18n_reaction_count` and the requested `post_id`.
///
/// State is retained across calls so each byte of the streamed body is examined
/// at most once: completed-but-non-matching scripts are skipped past, and the
/// search for an unfinished script's `</script>` resumes near the tail rather
/// than restarting. All markers are ASCII, and `post_id` is all-digits (the
/// caller only uses partial fetch for numeric post ids), so byte search is safe.
#[derive(Default)]
struct PostBlockScanner {
    /// Byte offset to resume the search for the next `<script` open tag.
    search_from: usize,
    /// Set while inside a script whose `</script>` has not arrived yet.
    open: Option<OpenScript>,
}

struct OpenScript {
    /// Index just past the `>` of the open tag (start of the script body).
    body_start: usize,
    /// Whether the open tag matched the JSON-block attributes.
    is_json_block: bool,
    /// Byte offset to resume the search for `</script>`.
    close_search_from: usize,
}

impl PostBlockScanner {
    fn found_match(&mut self, html: &[u8], post_id: &str) -> bool {
        const CLOSE: &[u8] = b"</script>";
        loop {
            if let Some(open) = self.open.as_mut() {
                match find_bytes(&html[open.close_search_from..], CLOSE) {
                    Some(rel) => {
                        let close_start = open.close_search_from + rel;
                        if open.is_json_block {
                            let block = &html[open.body_start..close_start];
                            if contains_bytes(block, b"i18n_reaction_count")
                                && contains_bytes(block, post_id.as_bytes())
                            {
                                return true;
                            }
                        }
                        self.search_from = close_start + CLOSE.len();
                        self.open = None;
                        // fall through to look for the next script
                    }
                    None => {
                        // Not closed yet. Resume near the tail next time, backing
                        // up by CLOSE.len()-1 so a `</script>` split across a
                        // chunk boundary is still found. Never go before body_start.
                        open.close_search_from = html
                            .len()
                            .saturating_sub(CLOSE.len() - 1)
                            .max(open.body_start);
                        return false;
                    }
                }
            } else {
                let Some(rel) = find_bytes(&html[self.search_from..], b"<script") else {
                    self.search_from = html.len();
                    return false;
                };
                let tag_start = self.search_from + rel;
                let Some(gt_rel) = find_bytes(&html[tag_start..], b">") else {
                    // Open tag not finished yet; re-examine from here next chunk.
                    self.search_from = tag_start;
                    return false;
                };
                let body_start = tag_start + gt_rel + 1;
                let open_tag = &html[tag_start..body_start];
                let is_json_block = contains_bytes(open_tag, br#"type="application/json""#)
                    && contains_bytes(open_tag, b"data-content-len")
                    && contains_bytes(open_tag, b"data-sjs");
                self.open = Some(OpenScript {
                    body_start,
                    is_json_block,
                    close_search_from: body_start,
                });
                // loop to try to close this script
            }
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    find_bytes(haystack, needle).is_some()
}
```

**Verify**: `cargo build` → exit 0, no errors.

### Step 3: Update the predicate's unit test

In `src/parsers/json_post.rs`, the `tests` module imports
`has_completed_matching_post_block` (line ~256) and uses it in
`matching_post_block_requires_completed_script` (lines 314-330). Replace that
import and test so they exercise the new scanner. The test must assert the same
three behaviors as before (complete+matching ⇒ true; incomplete ⇒ false;
complete+non-matching id ⇒ false), plus one new case proving the scanner works
when the body is delivered in two pieces (the regression this plan targets).

Update the `use super::{...}` line to drop `has_completed_matching_post_block`
(it no longer exists) and add `PostBlockScanner`. Then replace the test body:

```rust
    #[test]
    fn matching_post_block_requires_completed_script() {
        let open = r#"<script type="application/json" data-content-len="42" data-sjs>"#;
        let body = r#"{"i18n_reaction_count":"1K","id":"123"}"#;

        // Open tag + body but no `</script>` yet → not done.
        let mut s = PostBlockScanner::default();
        assert!(!s.found_match(format!("{open}{body}").as_bytes(), "123"));

        // Completed script whose body matches the id → done.
        let mut s = PostBlockScanner::default();
        assert!(s.found_match(
            format!("{open}{body}</script><div>later</div>").as_bytes(),
            "123"
        ));

        // Completed script, but the requested id is absent → not done.
        let mut s = PostBlockScanner::default();
        assert!(!s.found_match(format!("{open}{body}</script>").as_bytes(), "456"));
    }

    #[test]
    fn scanner_matches_across_chunk_boundaries() {
        let open = r#"<script type="application/json" data-content-len="42" data-sjs>"#;
        let body = r#"{"i18n_reaction_count":"1K","id":"123"}"#;
        let full = format!("{open}{body}</script>");
        let bytes = full.as_bytes();

        // Feed the same buffer growing one byte at a time (simulating chunks).
        // It must return false until the closing tag is present, then true,
        // and must never return true before the `</script>` arrives.
        let mut scanner = PostBlockScanner::default();
        let mut fired_at = None;
        for end in 1..=bytes.len() {
            if scanner.found_match(&bytes[..end], "123") {
                fired_at = Some(end);
                break;
            }
        }
        assert_eq!(fired_at, Some(bytes.len()));
    }
```

**Verify**: `cargo test --locked json_post` → all `json_post` tests pass, including
the two above. Then `cargo test --locked` → the whole suite passes.

### Step 4: Format

Run `cargo fmt`, then confirm clean.

**Verify**: `cargo fmt --check` → exit 0.

## Test plan

- Update `matching_post_block_requires_completed_script` (json_post.rs tests) to
  drive `PostBlockScanner::found_match` over byte slices — same three assertions
  as the original test (complete+match ⇒ true, incomplete ⇒ false,
  complete+wrong-id ⇒ false).
- Add `scanner_matches_across_chunk_boundaries` — grows the buffer one byte at a
  time and asserts the match fires exactly when the closing `</script>` is
  present and not before. This is the direct regression test for the streaming
  correctness of the rewrite.
- Structural pattern to follow: the existing `#[cfg(test)] mod tests` in
  `src/parsers/json_post.rs`.
- Verification: `cargo test --locked` → all pass; the two json_post predicate
  tests above included.

## Done criteria

ALL must hold:

- [ ] `cargo build` exits 0.
- [ ] `cargo test --locked` exits 0; the json_post predicate tests pass, including
      the new `scanner_matches_across_chunk_boundaries`.
- [ ] `cargo fmt --check` exits 0.
- [ ] `grep -n "from_utf8_lossy(&body)" src/fetch.rs` returns exactly **one** match
      (the single post-loop decode at the end of `fetch_until`), not two.
- [ ] `grep -n "has_completed_matching_post_block" src` returns no matches (old
      predicate fully removed).
- [ ] `grep -rn "fetch_until" src` shows only the definition in `src/fetch.rs`
      and the one call in `src/parsers/json_post.rs`.
- [ ] `git status` shows only `src/fetch.rs` and `src/parsers/json_post.rs`
      modified.
- [ ] `plans/README.md` status row for 001 updated.

## STOP conditions

Stop and report back (do not improvise) if:

- The drift check shows `src/fetch.rs` or `src/parsers/json_post.rs` changed
  since `3b6521c` and the "Current state" excerpts no longer match the live code.
- `grep -rn "fetch_until" src` shows a caller other than `json_post.rs` — the
  closure-signature change would then break code not covered by this plan; report
  the extra call site instead of guessing.
- You cannot make `matching_post_block_requires_completed_script` pass with the
  new scanner while keeping its three original assertions — the byte scanner is
  not reproducing the old stop decisions, which is a correctness regression.
- Any test outside `json_post` starts failing — that implies the `fetch_until`
  loop change altered behavior beyond this path; report it.

## Maintenance notes

- **Do this plan before plan 003.** Plan 003 also edits `fetch_until` (it adds
  HTTP-status capture and classification around the same loop). Landing 001 first
  avoids a conflict; 003's excerpts assume the loop shape produced here.
- The scanner assumes `post_id` is ASCII (guaranteed today by
  `should_try_partial_fetch` requiring all-digit ids). If a future change enables
  partial fetch for non-ASCII ids, `post_id.as_bytes()` substring matching still
  works byte-wise, but revisit whether that is the intended match semantics.
- Reviewer should scrutinize the boundary-resume arithmetic in `found_match`
  (the `saturating_sub(CLOSE.len() - 1).max(body_start)`), which is what keeps a
  `</script>` split across two chunks detectable.
- This plan deliberately does not touch `Fetcher::fetch` (the non-streaming path)
  or any DOM parsing; it only removes per-chunk redundant work.
