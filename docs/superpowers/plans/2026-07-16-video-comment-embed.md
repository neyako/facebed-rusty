# Video Comment Embed + Page-Video Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `?comment_id=` Facebook URLs embed the comment's content (text/image/video, author with 💬 suffix), and fix the failing plain page-video links like `/61576847095159/videos/1589861246186915/`.

**Architecture:** New `CommentParser` (`src/parsers/comment.rs`) dispatched when the cleaned URL query carries `comment_id`; on comment-not-found it falls back to the underlying post/video parser. Page-video fix is recon-driven: a new `--dump` CLI flag captures raw HTML + JSON blocks, then the missing key probe is added where the parser fails.

**Tech Stack:** Rust 1.75+, axum, reqwest, scraper, serde_json. NO new dependencies (base64 decode is hand-rolled, ~15 lines).

**Spec:** `docs/superpowers/specs/2026-07-16-video-comment-embed-design.md`

## Global Constraints

- Never index FB JSON by hardcoded path — search by key via `jq::first/all/has` (AGENTS.md rule).
- Error code letters `C/P/U/X` are user-visible; do not rename.
- Error tags for this feature: `(ccn)` comment node not found → NoData, `(cau)` author missing → Parse.
- Errors that should reach the Discord triage webhook are built with `FacebedError::parse_with(msg, page.html.clone(), page.url.clone())`.
- No new crates in `Cargo.toml`.
- Commit style: short imperative subject, no Conventional Commits prefix, body only when "why" isn't obvious.
- All commands run from repo root. `cargo test` must pass at the end of every task.
- Known recon fact: anonymous fetch of the example video page returns a login-preloader page that still contains `i18n_reaction_count`, so `probe_page_type` says HasData and `VideoWatchParser` dies at `(cn)`. The with-cookies shape is unknown until Task 2 runs on the deployment machine with a valid `cookies.json`.

---

### Task 1: `--dump` recon flag

**Files:**
- Modify: `src/main.rs` (Args struct ~line 29-39, main() after fetcher construction ~line 58)

**Interfaces:**
- Produces: `facebed --dump <fb-path> [--dump-dir <dir>]` — fetches the path through the production `Fetcher` (cookies + UA + probe), writes `page.html` and `block_NNN.json` files, prints summary, exits. Used by Task 2.
- Consumes: existing `Fetcher::fetch(path, true)`, `fetch::get_json_blocks(&Html, bool)`.

- [ ] **Step 1: Add the flags to `Args`**

In `src/main.rs`, extend the `Args` struct:

```rust
#[derive(Parser, Debug)]
#[command(name = "facebed", about = "Facebook embed proxy server")]
struct Args {
    /// Path to config YAML file (optional — defaults are used if omitted).
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Path to cookies.json (default: ./cookies.json).
    #[arg(long, default_value = "cookies.json")]
    cookies: PathBuf,

    /// Recon: fetch one Facebook path, dump raw HTML + JSON blocks, exit.
    #[arg(long, value_name = "FB_PATH")]
    dump: Option<String>,

    /// Output directory for --dump.
    #[arg(long, default_value = "/tmp/recon_out")]
    dump_dir: PathBuf,
}
```

- [ ] **Step 2: Add the dump branch in `main()`**

Immediately after `let fetcher = Arc::new(Fetcher::new(cookies.clone())?);` and before `let notifier = ...`, insert:

```rust
    if let Some(fb_path) = args.dump.as_deref() {
        let page = fetcher
            .fetch(fb_path, true)
            .await
            .map_err(|e| anyhow::anyhow!("dump fetch failed: {e}"))?;
        std::fs::create_dir_all(&args.dump_dir)?;
        std::fs::write(args.dump_dir.join("page.html"), &page.html)?;
        let blocks = crate::fetch::get_json_blocks(page.document(), true);
        for (i, block) in blocks.iter().enumerate() {
            std::fs::write(
                args.dump_dir.join(format!("block_{i:03}.json")),
                serde_json::to_string_pretty(block)?,
            )?;
        }
        println!(
            "dumped {} json blocks from {} into {}",
            blocks.len(),
            page.url,
            args.dump_dir.display()
        );
        return Ok(());
    }
```

Note: `Fetcher::fetch` raises `NoData` on login walls — a dump that fails with
"login wall" is itself a valid recon result (bad/expired cookies).

- [ ] **Step 3: Verify it compiles and the flag exists**

Run: `cargo build 2>&1 | tail -3`
Expected: compiles with no errors.

Run: `cargo run -- --help 2>&1 | grep -A1 dump`
Expected: `--dump <FB_PATH>` and `--dump-dir` listed.

- [ ] **Step 4: Smoke-run (network, no cookies needed — expect either dump or clean error)**

Run: `cargo run -- --cookies cookies.example.json --dump '61576847095159/videos/1589861246186915/' --dump-dir /tmp/recon_out 2>&1 | tail -2`
Expected: either `dumped N json blocks ...` or `Error: dump fetch failed: ...` — both acceptable here; no panic.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "Add --dump recon flag for capturing FB page JSON"
```

---

### Task 2: Recon — capture with-cookies HTML for both workstreams

**Requires the deployment machine's `cookies.json`** (not in this repo). If executing on a machine without valid cookies, ask the operator to run the two dump commands below and hand back the output directories; do not guess shapes.

**Files:**
- Create: `docs/superpowers/plans/2026-07-16-recon-notes.md` (findings; committed for later tasks)

**Interfaces:**
- Produces: recon notes documenting (a) which `VideoWatchParser` stage fails for the page-video URL and what key the fix probe needs, (b) the exact comment-node shape: id fields, body key, author keys, reaction-count key, attachment media keys. Tasks 5 and 7 consume these notes.

- [ ] **Step 1: Dump the failing page-video**

```bash
cargo run -- --cookies /path/to/real/cookies.json \
  --dump '61576847095159/videos/1589861246186915/' --dump-dir /tmp/recon_video
```

- [ ] **Step 2: Dump a comment permalink on that video**

Pick any comment id from the video dump (`grep -rl 'comment' /tmp/recon_video/block_*.json`, look for numeric `legacy_fbid` values or `comment_id=` inside url strings), then:

```bash
cargo run -- --cookies /path/to/real/cookies.json \
  --dump '61576847095159/videos/1589861246186915/?comment_id=<ID>' --dump-dir /tmp/recon_comment
```

- [ ] **Step 3: Diagnose the page-video failure stage**

```bash
grep -l 'video_view_count_renderer' /tmp/recon_video/block_*.json | head -3   # content node present?
grep -l 'progressive_url\|playable_url\|browser_native' /tmp/recon_video/block_*.json | head -3  # video link present?
grep -l 'creation_time' /tmp/recon_video/block_*.json | head -3               # date present?
```

Decision:
- No `video_view_count_renderer` hit → failure is `(cn)`: record which key pair DOES identify the video data block (inspect the largest blocks; candidates: `comment_rendering_instance` alone, `videoDeliveryResponseFragment` + `title`, `video_home_www_injected_video`).
- Video-link greps empty → failure is `(vn)`: record the key that holds the mp4 URL.
- No `creation_time` → date probe: record replacement key (`publish_time`, `created_time`).

- [ ] **Step 4: Document the comment node shape**

```bash
grep -l 'preferred_body' /tmp/recon_comment/block_*.json
```

Open the matching block, find the comment matching `<ID>`, and record in the notes file: path context (e.g. `comment_rendering_instance.comments.edges[].node`), the id fields present (`id` base64? `legacy_fbid`?), body key (`preferred_body.text`?), author keys (`author.name`, `author.id`), timestamp key, reaction count key (`reactors.count`? `unified_reactors.count`?), and — if the chosen comment has media — the attachment keys around the image/video.

- [ ] **Step 5: Write and commit recon notes**

`docs/superpowers/plans/2026-07-16-recon-notes.md` — a short bullet list per Step 3/4 finding. Copy one representative (redacted if needed) comment-node JSON snippet into the notes.

```bash
git add docs/superpowers/plans/2026-07-16-recon-notes.md
git commit -m "Record recon findings for comment embed and page-video fix"
```

---

### Task 3: Preserve query string in the `/videos/<id>` → `reel/<id>` rewrite

**Files:**
- Modify: `src/routes.rs:421-424` (the RE_VIDEOS rewrite in `catch_all`)
- Test: `src/routes.rs` tests module (bottom of file, near the `scope_key` tests)

**Interfaces:**
- Produces: `rewrite_videos_path(working: &str) -> Option<String>` — returns the rewritten path (query preserved) when `RE_VIDEOS` matches. `catch_all` calls it. Task 4's dispatch relies on `comment_id` surviving this rewrite.

- [ ] **Step 1: Write the failing test**

Add to the routes tests module:

```rust
#[test]
fn videos_rewrite_preserves_query() {
    assert_eq!(
        rewrite_videos_path("videos/123/?comment_id=456"),
        Some("reel/123?comment_id=456".to_string())
    );
    assert_eq!(rewrite_videos_path("videos/123/"), Some("reel/123".to_string()));
    assert_eq!(rewrite_videos_path("reel/123"), None);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test videos_rewrite_preserves_query 2>&1 | tail -5`
Expected: FAIL — `rewrite_videos_path` not found.

- [ ] **Step 3: Implement**

Replace the inline rewrite in `catch_all`:

```rust
    // /videos/<id> → reel/<id>
    if let Some(rewritten) = rewrite_videos_path(&working) {
        working = rewritten;
    }
```

Add next to the other free functions in routes.rs:

```rust
/// `videos/<slug?>/<id>[?query]` → `reel/<id>[?query]`. Query survives so
/// `comment_id` dispatch (ParserKind::Comment) still sees it.
fn rewrite_videos_path(working: &str) -> Option<String> {
    let caps = RE_VIDEOS.captures(working)?;
    let mut out = format!("reel/{}", &caps[1]);
    if let Some((_, query)) = working.split_once('?') {
        if !query.is_empty() {
            out.push('?');
            out.push_str(query);
        }
    }
    Some(out)
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test 2>&1 | tail -3`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add src/routes.rs
git commit -m "Preserve query string when rewriting videos path to reel"
```

---

### Task 4: `CommentParser` — node location and matching

**Files:**
- Create: `src/parsers/comment.rs`
- Modify: `src/parsers/mod.rs` (add `pub mod comment;`)

**Interfaces:**
- Produces: `CommentParser` implementing `Parser`; helpers `comment_id_in(path: &str) -> Option<String>`, `find_comment_node<'a>(blocks: &'a [Value], comment_id: &str) -> Option<&'a Value>`, `b64_decode_ascii(s: &str) -> Option<String>` (all in `comment.rs`; `comment_id_in` is `pub(crate)` — routes.rs uses it in Task 5).
- Consumes: `Parser` trait (`parsers/mod.rs:60-63`), `get_json_blocks` (fetch.rs), `jq`, `util::{human_format, images_from_post, videos_from_post, video_link_in_node, thumbnail_in_node, val_str_at}`.

**Key-name caveat:** the field names below (`preferred_body`, `author`, `legacy_fbid`, `created_time`, `reactors`) follow the photocom precedent and standard FB comet shapes. Cross-check every one against `docs/superpowers/plans/2026-07-16-recon-notes.md` from Task 2 and adjust before finishing this task. The *structure* of the code stays as written.

- [ ] **Step 1: Write the failing tests**

Create `src/parsers/comment.rs` with only the tests module first:

```rust
use crate::error::{FacebedError, FacebedResult};
use crate::fetch::get_json_blocks;
use crate::jq;
use crate::parsers::util::{
    human_format, images_from_post, thumbnail_in_node, val_str_at, video_link_in_node,
    videos_from_post,
};
use crate::parsers::{banned_post, ParsedPost, Parser, ParserCtx};
use crate::url_clean::ensure_absolute;
use serde_json::Value;
use url::Url;

pub struct CommentParser;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn comment_block() -> Value {
        json!({
            "comment_rendering_instance": {
                "comments": {
                    "edges": [
                        {"node": {
                            "id": "Y29tbWVudDo5OTlfMTEx",           // base64("comment:999_111")
                            "legacy_fbid": "111",
                            "author": {"name": "Alice", "id": "42"},
                            "preferred_body": {"text": "first!"},
                            "created_time": 1750000000,
                            "feedback": {"url": "https://www.facebook.com/x?comment_id=111"},
                            "reactors": {"count": 7}
                        }},
                        {"node": {
                            "id": "Y29tbWVudDo5OTlfMjIy",           // base64("comment:999_222")
                            "author": {"name": "Bob", "id": "43"},
                            "preferred_body": {"text": "video reply"},
                            "created_time": 1750000100,
                            "attachments": [{"style_type_renderer": {"attachment": {"media": {
                                "videoDeliveryResponseFragment": {
                                    "videoDeliveryResponseResult": {
                                        "progressive_urls": [
                                            {"progressive_url": "https://video.fbcdn.net/c.mp4"}
                                        ]
                                    }
                                },
                                "preferred_thumbnail": {"image": {"uri": "https://img.fbcdn.net/t.jpg"}}
                            }}}}]
                        }}
                    ]
                }
            }
        })
    }

    #[test]
    fn b64_decodes_comment_ids() {
        assert_eq!(
            b64_decode_ascii("Y29tbWVudDo5OTlfMTEx").as_deref(),
            Some("comment:999_111")
        );
        assert_eq!(b64_decode_ascii("!!!"), None);
    }

    #[test]
    fn comment_id_in_reads_query() {
        assert_eq!(
            comment_id_in("reel/999?comment_id=111").as_deref(),
            Some("111")
        );
        assert_eq!(comment_id_in("reel/999"), None);
        assert_eq!(comment_id_in("reel/999?comment_id="), None);
    }

    #[test]
    fn finds_comment_by_legacy_fbid() {
        let blocks = vec![comment_block()];
        let node = find_comment_node(&blocks, "111").unwrap();
        assert_eq!(
            node.pointer("/preferred_body/text").and_then(|v| v.as_str()),
            Some("first!")
        );
    }

    #[test]
    fn finds_comment_by_base64_id_when_no_legacy_fbid() {
        let blocks = vec![comment_block()];
        let node = find_comment_node(&blocks, "222").unwrap();
        assert_eq!(
            node.pointer("/preferred_body/text").and_then(|v| v.as_str()),
            Some("video reply")
        );
    }

    #[test]
    fn missing_comment_returns_none() {
        let blocks = vec![comment_block()];
        assert!(find_comment_node(&blocks, "333").is_none());
    }
}
```

- [ ] **Step 2: Declare module, run tests, verify failure**

Add `pub mod comment;` to `src/parsers/mod.rs` (alphabetical, before `json_post`).

Run: `cargo test comment:: 2>&1 | tail -5`
Expected: FAIL — `b64_decode_ascii`, `comment_id_in`, `find_comment_node` not found.

- [ ] **Step 3: Implement the helpers**

Add above the tests module in `comment.rs`:

```rust
/// Base64 (standard or URL-safe alphabet) → ASCII string. FB comment `id`
/// fields are base64 of `comment:<post_fbid>_<comment_fbid>`.
/// ponytail: hand-rolled to avoid a base64 crate for one call site.
fn b64_decode_ascii(s: &str) -> Option<String> {
    let mut bits: u32 = 0;
    let mut n = 0u32;
    let mut out = Vec::new();
    for &c in s.trim_end_matches('=').as_bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32;
        bits = (bits << 6) | v;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
        }
    }
    String::from_utf8(out).ok()
}

/// The `comment_id` query value, if present and non-empty.
pub(crate) fn comment_id_in(path: &str) -> Option<String> {
    let url = Url::parse(&ensure_absolute(path)).ok()?;
    url.query_pairs()
        .find(|(k, _)| k == "comment_id")
        .map(|(_, v)| v.into_owned())
        .filter(|v| !v.is_empty())
}

/// Any object with a comment body and an author is a candidate comment node.
fn candidate_comment_nodes(bloc: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    for edges in jq::all(bloc, "edges") {
        let Some(arr) = edges.as_array() else { continue };
        for edge in arr {
            if let Some(node) = edge.get("node") {
                if node.get("preferred_body").is_some() && node.get("author").is_some() {
                    out.push(node);
                }
            }
        }
    }
    // Single highlighted-comment shapes outside edges lists.
    for key in ["comment", "attached_comment"] {
        for node in jq::all(bloc, key) {
            if node.get("preferred_body").is_some() && node.get("author").is_some() {
                out.push(node);
            }
        }
    }
    out
}

fn node_matches_id(node: &Value, comment_id: &str) -> bool {
    match node.get("legacy_fbid") {
        Some(Value::String(s)) if s == comment_id => return true,
        Some(Value::Number(n)) if n.to_string() == comment_id => return true,
        _ => {}
    }
    let needle = format!("comment_id={comment_id}");
    for url_val in jq::all(node, "url") {
        if url_val.as_str().is_some_and(|s| s.contains(&needle)) {
            return true;
        }
    }
    if let Some(id) = node.get("id").and_then(|v| v.as_str()) {
        if let Some(decoded) = b64_decode_ascii(id) {
            if decoded.starts_with("comment:") && decoded.ends_with(&format!("_{comment_id}")) {
                return true;
            }
        }
    }
    false
}

fn find_comment_node<'a>(blocks: &'a [Value], comment_id: &str) -> Option<&'a Value> {
    blocks
        .iter()
        .flat_map(|b| candidate_comment_nodes(b))
        .find(|n| node_matches_id(n, comment_id))
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test comment:: 2>&1 | tail -5`
Expected: 5 tests PASS. (`Parser` impl comes in the next task — an unused-import warning for `Parser`/`ParsedPost` etc. is fine at this checkpoint; silence with the Task 5 impl, not with `#[allow]`.)

- [ ] **Step 5: Commit**

```bash
git add src/parsers/comment.rs src/parsers/mod.rs
git commit -m "Add comment node location and id matching"
```

---

### Task 5: `CommentParser::process` — extraction into `ParsedPost`

**Files:**
- Modify: `src/parsers/comment.rs`

**Interfaces:**
- Produces: `impl Parser for CommentParser` — `process(ctx, post_path)` returns `ParsedPost` with `author_name = "{name} (💬)"`, or `FacebedError::NoData` (tag `(ccn)`) when the comment isn't server-rendered — Task 6's fallback keys off that variant.
- Consumes: helpers from Task 4; `util` fns; `ParserCtx::is_banned`.

- [ ] **Step 1: Write the failing test**

Add to the tests module in `comment.rs`:

```rust
    #[test]
    fn extracts_parsed_post_fields_from_comment_node() {
        let blocks = vec![comment_block()];
        let node = find_comment_node(&blocks, "222").unwrap();
        let post = parsed_post_from_comment(node, "reel/999?comment_id=222").unwrap();

        assert_eq!(post.author_name, "Bob (💬)");
        assert_eq!(post.text, "video reply");
        assert_eq!(post.date, 1750000100);
        assert_eq!(post.video_links, vec!["https://video.fbcdn.net/c.mp4".to_string()]);
        assert_eq!(post.thumbnail.as_deref(), Some("https://img.fbcdn.net/t.jpg"));
        assert!(post.image_links.is_empty());
        assert_eq!(post.url, "https://www.facebook.com/reel/999?comment_id=222");
    }

    #[test]
    fn text_only_comment_uses_feedback_url_and_reactions() {
        let blocks = vec![comment_block()];
        let node = find_comment_node(&blocks, "111").unwrap();
        let post = parsed_post_from_comment(node, "reel/999?comment_id=111").unwrap();

        assert_eq!(post.author_name, "Alice (💬)");
        assert_eq!(post.text, "first!");
        assert_eq!(post.likes, "7");
        assert_eq!(post.url, "https://www.facebook.com/x?comment_id=111");
        assert!(post.video_links.is_empty());
    }
```

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test comment:: 2>&1 | tail -5`
Expected: FAIL — `parsed_post_from_comment` not found.

- [ ] **Step 3: Implement extraction + the `Parser` impl**

```rust
/// Comment permalink URL: the node's own feedback URL when FB provides one
/// (it points at the comment, not just the post), else the request URL.
fn comment_url(node: &Value, post_path: &str) -> String {
    for url_val in jq::all(node, "url") {
        if let Some(s) = url_val.as_str() {
            if s.starts_with("https://") && s.contains("comment_id=") {
                return s.to_owned();
            }
        }
    }
    ensure_absolute(post_path)
}

fn comment_reactions(node: &Value) -> Value {
    for key in ["reactors", "unified_reactors"] {
        if let Some(count) = jq::first(node, key).and_then(|r| r.get("count")) {
            return count.clone();
        }
    }
    Value::Null
}

fn parsed_post_from_comment(node: &Value, post_path: &str) -> FacebedResult<ParsedPost> {
    let author = node
        .get("author")
        .ok_or_else(|| FacebedError::parse("comment author missing (cau)"))?;
    let author_name = val_str_at(author, "name")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| FacebedError::parse("comment author name missing (cau)"))?;
    let text = node
        .pointer("/preferred_body/text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let date = node
        .get("created_time")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let image_links = images_from_post(node);
    let mut video_links = videos_from_post(node);
    if video_links.is_empty() {
        if let Some(link) = video_link_in_node(node) {
            video_links.push(link);
        }
    }
    let thumbnail = if video_links.is_empty() {
        None
    } else {
        jq::all(node, "media")
            .into_iter()
            .find_map(thumbnail_in_node)
            .or_else(|| thumbnail_in_node(node))
    };

    Ok(ParsedPost {
        author_name: format!("{author_name} (💬)"),
        text,
        allow_discord_markdown: false,
        image_links,
        url: comment_url(node, post_path),
        date,
        likes: human_format(&comment_reactions(node)),
        comments: "null".into(),
        shares: "null".into(),
        video_links,
        thumbnail,
    })
}

#[async_trait::async_trait]
impl Parser for CommentParser {
    async fn process(&self, ctx: &ParserCtx, post_path: &str) -> FacebedResult<ParsedPost> {
        let comment_id = comment_id_in(post_path)
            .ok_or_else(|| FacebedError::parse("comment path without comment_id"))?;
        let page = ctx.fetcher.fetch(post_path, true).await?;
        let blocks = get_json_blocks(page.document(), true);
        let Some(node) = find_comment_node(&blocks, &comment_id) else {
            // Deep replies / stale ids aren't server-rendered. NoData (not
            // Parse) so routes falls back to the plain post embed. (ccn)
            return Err(FacebedError::no_data(format!(
                "comment {comment_id} not in server-rendered HTML (ccn)"
            )));
        };
        if let Some(author_id) = node.pointer("/author/id").and_then(|v| v.as_str()) {
            if ctx.is_banned(author_id) {
                return Ok(banned_post(&ensure_absolute(post_path)));
            }
        }
        parsed_post_from_comment(node, post_path).map_err(|e| match e {
            FacebedError::Parse { message, .. } => {
                FacebedError::parse_with(message, page.html.clone(), page.url.clone())
            }
            other => other,
        })
    }
}
```

Note on `human_format(&Value::Null)`: check `util::human_format` — `Value::Null`
formats as the string `"null"`, which is exactly what the embed renderer treats
as "no count"; that is the existing photocom behavior, keep it.

- [ ] **Step 4: Run tests**

Run: `cargo test comment:: 2>&1 | tail -5`
Expected: 7 tests PASS, no warnings about unused imports left.

- [ ] **Step 5: Reconcile key names with recon notes**

Read `docs/superpowers/plans/2026-07-16-recon-notes.md`. For every key that differs from the code (`preferred_body`, `author`, `legacy_fbid`, `created_time`, `reactors`, attachment media shape), update BOTH the code probes and the test fixture. If recon shows an additional id or reaction-count key, add it to the existing probe lists (`node_matches_id`, `comment_reactions`) — do not replace the current ones.

Run: `cargo test 2>&1 | tail -3`
Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add src/parsers/comment.rs
git commit -m "Extract comment content into ParsedPost"
```

---

### Task 6: Routing — dispatch `ParserKind::Comment` + fallback to post embed

**Files:**
- Modify: `src/routes.rs` — `ParserKind` enum (~line 395), dispatch block in `catch_all` (~line 426-446), `run_parser` (~line 570), plus new helper `strip_comment_id`
- Test: routes tests module

**Interfaces:**
- Consumes: `crate::parsers::comment::{comment_id_in, CommentParser}` (Task 4/5).
- Produces: `select_kind(working: &str) -> Option<ParserKind>` (extracted dispatch chain, reused by the fallback), `strip_comment_id(path: &str) -> String`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn strip_comment_id_removes_only_comment_params() {
    assert_eq!(strip_comment_id("reel/999?comment_id=111"), "reel/999");
    assert_eq!(
        strip_comment_id("story.php?story_fbid=1&comment_id=2&id=3"),
        "story.php?story_fbid=1&id=3"
    );
    assert_eq!(
        strip_comment_id("reel/999?comment_id=1&reply_comment_id=2"),
        "reel/999"
    );
}

#[test]
fn select_kind_routes_paths() {
    assert!(matches!(select_kind("reel/123"), Some(ParserKind::Reels)));
    assert!(matches!(select_kind("watch?v=1"), Some(ParserKind::Watch)));
    assert!(matches!(
        select_kind("groups/1/posts/2"),
        Some(ParserKind::JsonPost)
    ));
    assert!(select_kind("definitely-not-facebook").is_none());
}
```

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test strip_comment_id 2>&1 | tail -5 && cargo test select_kind_routes_paths 2>&1 | tail -5`
Expected: FAIL — functions not found.

- [ ] **Step 3: Implement**

Add `Comment` to the enum:

```rust
#[derive(Clone, Copy, Debug)]
enum ParserKind {
    JsonPost,
    SinglePhoto,
    Photocom,
    Reels,
    Watch,
    Stories,
    Comment,
}
```

Extract the existing dispatch chain from `catch_all` into a free function (verbatim move of the current if/else chain, returning `None` instead of the error-embed arm):

```rust
fn select_kind(working: &str) -> Option<ParserKind> {
    if RE_STORIES.is_match(working) {
        Some(ParserKind::Stories)
    } else if RE_REEL.is_match(working) {
        Some(ParserKind::Reels)
    } else if path_only(working)
        .map(|p| RE_PHOTO.is_match(&p))
        .unwrap_or(false)
    {
        Some(ParserKind::SinglePhoto)
    } else if path_only(working)
        .map(|p| RE_WATCH.is_match(&p))
        .unwrap_or(false)
        || RE_PAGE_VIDEO.is_match(working)
    {
        Some(ParserKind::Watch)
    } else if is_facebook_url(working) {
        Some(ParserKind::JsonPost)
    } else {
        None
    }
}
```

Rewrite the dispatch block in `catch_all` (keep the same position, right after the videos rewrite):

```rust
    // dispatch — comment permalinks first, then path-shape routing
    let kind = if crate::parsers::comment::comment_id_in(&working).is_some() {
        ParserKind::Comment
    } else {
        match select_kind(&working) {
            Some(kind) => kind,
            None => return html_response(format_error_embed("https://git.facebed.com", "C")),
        }
    };
```

Add the fallback helper near `select_kind`:

```rust
/// Drop comment_id/reply_comment_id so a failed comment lookup can re-dispatch
/// as the plain post/video it hangs off.
fn strip_comment_id(path: &str) -> String {
    let Ok(url) = Url::parse(&url_clean::ensure_absolute(path)) else {
        return path.to_owned();
    };
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "comment_id" && k != "reply_comment_id")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut out = url.path().trim_start_matches('/').to_owned();
    if !kept.is_empty() {
        let mut tmp = Url::parse("https://www.facebook.com").unwrap();
        {
            let mut qp = tmp.query_pairs_mut();
            for (k, v) in &kept {
                qp.append_pair(k, v);
            }
        }
        if let Some(q) = tmp.query() {
            out.push('?');
            out.push_str(q);
        }
    }
    out
}
```

Extend `run_parser` (recursion needs `Box::pin`):

```rust
async fn run_parser(
    state: &AppState,
    path: &str,
    kind: ParserKind,
) -> Result<ParsedPost, FacebedError> {
    match kind {
        ParserKind::JsonPost => JsonPostParser.process(&state.ctx, path).await,
        ParserKind::SinglePhoto => SinglePhotoParser.process(&state.ctx, path).await,
        ParserKind::Photocom => PhotocomParser.process(&state.ctx, path).await,
        ParserKind::Reels => ReelsParser.process(&state.ctx, path).await,
        ParserKind::Watch => VideoWatchParser.process(&state.ctx, path).await,
        ParserKind::Stories => StoriesParser.process(&state.ctx, path).await,
        ParserKind::Comment => {
            match CommentParser.process(&state.ctx, path).await {
                Err(FacebedError::NoData(reason)) => {
                    // Comment absent from SSR HTML — embed the underlying post
                    // instead of erroring with C.
                    let stripped = strip_comment_id(path);
                    let Some(fallback) = select_kind(&stripped) else {
                        return Err(FacebedError::no_data(reason));
                    };
                    info!(
                        path = %stripped,
                        kind = ?fallback,
                        %reason,
                        "comment not found; falling back to post embed"
                    );
                    Box::pin(run_parser(state, &stripped, fallback)).await
                }
                other => other,
            }
        }
    }
}
```

Import `CommentParser` alongside the other parser imports at the top of routes.rs, and check the `FacebedError::NoData` variant name against `src/error.rs` (`no_data` constructor exists; confirm the variant is `NoData(String)` or adjust the pattern to match, e.g. `NoData(_)` with `reason` re-created).

- [ ] **Step 4: Run all tests**

Run: `cargo test 2>&1 | tail -3`
Expected: all PASS.

Check `render()`/`render_with_size_check()` need no change: `ParserKind::Comment` is not in `force_reel`, so a video comment renders as a video card via the existing `video_only` branch, and image/text comments get the full-post card. Confirm by reading `render()` (~routes.rs:602) — no code change expected.

- [ ] **Step 5: Commit**

```bash
git add src/routes.rs
git commit -m "Dispatch comment permalinks with fallback to post embed"
```

---

### Task 7: Fix the plain page-video failure (recon-gated)

**Files:**
- Modify: exactly ONE of the following, per Task 2's diagnosis:
  - `(cn)` → `src/parsers/video_watch.rs::get_content_node` (~line 221)
  - `(vn)` → `src/parsers/util.rs::video_link_in_node` (~line 224)
  - date → `src/parsers/video_watch.rs::find_creation_time` (~line 272)
- Test: same file's tests module

**Interfaces:**
- Consumes: `docs/superpowers/plans/2026-07-16-recon-notes.md`.
- Produces: no signature changes — only an added probe inside the existing function.

- [ ] **Step 1: Write the failing test from the recon fixture**

Build a minimal `json!` fixture reproducing the discovered shape. Template for the most likely case, `(cn)` — the video data block exists but lacks `video_view_count_renderer`, carrying only `comment_rendering_instance` + a `videoDeliveryResponseFragment` sibling (SUBSTITUTE the actual keys from recon notes):

```rust
#[test]
fn content_node_accepts_page_video_shape() {
    let blocks = vec![json!({
        "result": {"data": {
            "id": "1589861246186915",
            "feedback": {"comment_rendering_instance": {}},
            "videoDeliveryResponseFragment": {}
        }}
    })];
    let html = Html::parse_document("");
    let content = get_content_node(&blocks, &html, "", "", Some("1589861246186915")).unwrap();
    assert_eq!(
        content.get("id").and_then(|v| v.as_str()),
        Some("1589861246186915")
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test content_node_accepts_page_video_shape 2>&1 | tail -5`
Expected: FAIL (Err from `get_content_node`).

- [ ] **Step 3: Add the probe**

Rules: ADD an alternative probe after the existing exact match — never loosen or remove the current one. For the `(cn)` template case, extend both the targeted and untargeted loops in `get_content_node` to also accept `jq::has(data, &["comment_rendering_instance", "videoDeliveryResponseFragment"])` as a fallback when the strict pair matched nothing. Follow the shape recorded in recon notes if it differs.

- [ ] **Step 4: Run all tests**

Run: `cargo test 2>&1 | tail -3`
Expected: all PASS.

- [ ] **Step 5: Live verification (deployment machine)**

```bash
cargo run -- --cookies /path/to/real/cookies.json &
sleep 2
curl -s -A 'Discordbot/2.0' 'localhost:9812/share/v/1HJXiXrBGD/' | grep -o 'og:video[^>]*' | head -2
kill %1
```

Expected: an `og:video` (or `og:image` thumbnail fallback) tag present; embed title does NOT contain `[C]`/`[P]`.

- [ ] **Step 6: Commit**

```bash
git add src/parsers/
git commit -m "Accept page-video JSON shape in watch parser"
```

---

### Task 8: End-to-end verification + docs

**Files:**
- Modify: `AGENTS.md` (Layout section parser list; Architecture → Parsers; Request flow dispatch list)

**Interfaces:** none — verification and documentation.

- [ ] **Step 1: Full test suite + clippy**

Run: `cargo test 2>&1 | tail -3 && cargo clippy 2>&1 | tail -3`
Expected: tests PASS; no new clippy warnings in touched files.

- [ ] **Step 2: Live verification matrix (deployment machine, real cookies)**

Start the server, then for each row `curl -s -A 'Discordbot/2.0' 'localhost:9812/<path>'` and check the OG tags:

| Path | Expect |
|---|---|
| `share/v/1HJXiXrBGD/` | video embed, no `[C]`/`[P]` in title |
| `<video path>?comment_id=<real id>` (from Task 2 recon) | `(💬)` in author, comment text in description |
| `<video path>?comment_id=<real id of a video comment>` | `og:video` pointing at the comment's clip |
| `<video path>?comment_id=999999999999` (bogus) | falls back to plain video embed, no error code |
| a known-good existing URL, e.g. a `reel/<id>` | unchanged behavior (regression check) |

- [ ] **Step 3: Update AGENTS.md**

Add to the Layout parser list: `comment.rs  comment permalink (?comment_id=) — text/image/video comment embed, falls back to post embed when comment not server-rendered`.
Add to Request flow: comment_id dispatch step (before path-shape routing, after /videos rewrite).
Add to Parsers section: a `CommentParser` bullet mirroring the others (id matching via legacy_fbid / url / base64 id; `(ccn)` NoData → fallback).

- [ ] **Step 4: Commit**

```bash
git add AGENTS.md
git commit -m "Document comment parser in AGENTS.md"
```
