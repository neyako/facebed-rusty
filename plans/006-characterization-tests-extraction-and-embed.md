# Plan 006: Add characterization tests for the JSON-extraction and embed-render path

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If a STOP condition occurs, stop and report. When done, update the
> status row for this plan in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat 3b6521c..HEAD -- src/parsers/util.rs src/embed.rs src/parsers/single_photo.rs src/parsers/photocom.rs src/parsers/stories.rs`
> These files may have gained a `#[cfg(test)] mod tests` from another plan since
> `3b6521c` — that is fine (append to it). If their **non-test** code differs
> from the excerpts in this plan, STOP.

## Status

- **Priority**: P2 (de-risks the behavior-changing plans)
- **Effort**: M
- **Risk**: LOW (test-only additions; no source logic changes)
- **Depends on**: none (but most valuable landed before/with plans 001, 002, 003)
- **Category**: tests
- **Planned at**: commit `3b6521c`, 2026-06-17

## Why this matters

The core job of facebed — turn Facebook JSON into a `ParsedPost` and render it as
OpenGraph meta tags — has **no characterization tests**. The existing suite tests
small helpers (id parsing, url cleaning, cookie bookkeeping), but the extraction
logic (`Story::from_json`, image/video extraction) and the embed renderers
(`format_full_post_embed`, `format_reel_post_embed`) are exercised only against
live Facebook HTML by hand. Three parsers (`single_photo`, `photocom`, `stories`)
have **zero** tests. That means the behavior-changing plans in this directory
(001 rewrites the fetch scan, 002 fixes image extraction, 003 changes error
handling) have no automated safety net against regressions in the embed output.

This plan adds fast, deterministic tests built from **synthetic** Facebook-shaped
JSON (hand-authored — no live data, no fixtures to capture), covering: `Story`
extraction (author / text / images / videos / shared-post recursion), the two
embed renderers (correct og: tags + attribute escaping), and the content-node
finder in each of the three untested parsers. After this, a regression in the
extraction or render path fails `cargo test`.

## Current state

The functions under test and their shapes (all confirmed in the source):

- `src/parsers/util.rs`:
  - `pub fn images_from_post(post_json: &Value) -> Vec<String>` (line 134) — for a
    single-media attachment, returns `photo_image.uri` (unless the media is a
    Sticker).
  - `pub fn videos_from_post(post_json: &Value) -> Vec<String>` (line 208) — walks
    each `attachment` via `video_link_in_node`, which reads
    `videoDeliveryResponseFragment...progressive_url`, legacy `browser_native_*`,
    or `playable_url`.
  - `impl Story<'a> { pub fn from_json(&Value) -> Result<Self, FacebedError>` (line 63)
    and `pub fn get_text(&self) -> String` (line 125) }`. `from_json` reads
    `actors[0].name`/`.id`, `message.text`, `wwwURL`, optional `attached_story`
    (recursed). `get_text` joins the post and its `attached_story` with
    `"\n╰┈➤ {author}\n{text}"`.
  - This file may or may not already have a `#[cfg(test)] mod tests` (another plan
    may have added one). If absent, create it; if present, append.
- `src/embed.rs`:
  - `pub fn format_full_post_embed(post: &ParsedPost, tz_offset: i32) -> String`
    (line 156) — image-card embed; `title`/`desc` go through `escape_attr`
    (HTML-attribute escaping), images become `og:image` meta tags.
  - `pub fn format_reel_post_embed(post: &ParsedPost, tz_offset: i32) -> String`
    (line 213) — video-card embed; emits `og:video` and `twitter:card=player`.
  - Existing `mod tests` (line 389) tests `format_description_text` only.
- `ParsedPost` (`src/parsers/mod.rs:14-30`) is a `pub struct` with all-`pub`
  fields, so a test can build one directly. A timestamp of `-1` renders as empty
  (`format_timestamp` returns `""` for `ts < 0`), so use `date: -1` to keep
  assertions timezone-independent.
- `src/parsers/single_photo.rs` — `fn get_content_node(blocks: &[Value]) -> Option<Value>`
  (line 70, matches `message_preferred_body` + `container_story`, returns the
  `data` node) and `fn get_single_image(blocks: &[Value]) -> Option<String>`
  (line 88, reads `prefetch_uris_v2[0].uri`). **No test module.**
- `src/parsers/photocom.rs` — `fn get_reaction_count(blocks: &[Value]) -> Option<i64>`
  (line 85) and `fn get_attached_image_and_url(blocks: &[Value]) -> Option<(String, String)>`
  (line 94). **No test module.**
- `src/parsers/stories.rs` — `fn find_story_bucket_and_node(blocks: &[Value]) -> Option<(Value, Value)>`
  (line 111). **No test module.**

### Convention to follow

- Tests use `serde_json::json!` to build fixtures (see `src/parsers/reels.rs:334`
  and `src/parsers/video_watch.rs:284` for the exact idiom — `use serde_json::json;`,
  `use super::{...};`).
- Private functions are tested from the same file's `mod tests` via `use super::...`.
- Hard gates: `cargo fmt --check` and `cargo test`.

## Commands you will need

| Purpose      | Command                          | Expected on success |
|--------------|----------------------------------|---------------------|
| Build        | `cargo build`                    | exit 0              |
| Tests        | `cargo test --locked`            | all pass            |
| Scoped tests | `cargo test --locked util`       | the new util tests pass |
| Format       | `cargo fmt`                      | reformats in place  |
| Format check | `cargo fmt --check`              | exit 0              |

## Scope

**In scope** (test modules only — do NOT change any non-test code):
- `src/parsers/util.rs`
- `src/embed.rs`
- `src/parsers/single_photo.rs`
- `src/parsers/photocom.rs`
- `src/parsers/stories.rs`

**Out of scope**:
- Any production (non-`#[cfg(test)]`) code in those files or anywhere else. If a
  test you write fails because the code looks buggy, do NOT fix the code — record
  it (STOP condition) so it can be triaged as a finding.
- Capturing real Facebook HTML fixtures — this plan uses synthetic JSON only.
- Network/integration tests — everything here is pure-function, in-memory.

## Git workflow

- Branch: `advisor/006-characterization-tests`.
- Commit style: short imperative subject (e.g. "Add extraction and embed tests").
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: `Story` extraction tests (`src/parsers/util.rs`)

Add to the file's `mod tests` (create `#[cfg(test)] mod tests { ... }` at the end
if none exists). Inside it, `use super::Story;` and `use serde_json::json;`.

```rust
    #[test]
    fn story_extracts_author_text_and_photo() {
        let story = Story::from_json(&json!({
            "actors": [{"name": "Test Author", "id": "100"}],
            "message": {"text": "hello world"},
            "wwwURL": "https://www.facebook.com/groups/1/posts/2",
            "attachment": {
                "media": {"__typename": "Photo"},
                "photo_image": {"uri": "https://img.example/p.jpg"}
            }
        }))
        .unwrap();
        assert_eq!(story.author_name, "Test Author");
        assert_eq!(story.author_id, "100");
        assert_eq!(story.text, "hello world");
        assert_eq!(story.url, "https://www.facebook.com/groups/1/posts/2");
        assert_eq!(story.image_links, vec!["https://img.example/p.jpg".to_string()]);
        assert!(story.video_links.is_empty());
    }

    #[test]
    fn story_extracts_progressive_video() {
        let story = Story::from_json(&json!({
            "actors": [{"name": "V", "id": "7"}],
            "message": {"text": "vid"},
            "wwwURL": "https://www.facebook.com/x",
            "attachment": {
                "media": {
                    "videoDeliveryResponseFragment": {
                        "videoDeliveryResponseResult": {
                            "progressive_urls": [
                                {"progressive_url": "https://video.fbcdn.net/v.mp4"}
                            ]
                        }
                    }
                }
            }
        }))
        .unwrap();
        assert_eq!(story.video_links, vec!["https://video.fbcdn.net/v.mp4".to_string()]);
    }

    #[test]
    fn story_appends_shared_attached_story_text() {
        let story = Story::from_json(&json!({
            "actors": [{"name": "Outer", "id": "1"}],
            "message": {"text": "outer text"},
            "wwwURL": "https://www.facebook.com/o",
            "attached_story": {
                "actors": [{"name": "Inner", "id": "2"}],
                "message": {"text": "inner text"}
            }
        }))
        .unwrap();
        let combined = story.get_text();
        assert!(combined.contains("outer text"));
        assert!(combined.contains("╰┈➤ Inner"));
        assert!(combined.contains("inner text"));
    }
```

**Verify**: `cargo test --locked util` → these pass (alongside any pre-existing
util tests).

### Step 2: Embed renderer tests (`src/embed.rs`)

Add to the existing `mod tests` in `src/embed.rs`. Add
`use super::{format_full_post_embed, format_reel_post_embed};` and
`use crate::parsers::ParsedPost;`.

```rust
    fn sample_post() -> ParsedPost {
        ParsedPost {
            author_name: r#"Title "quote""#.into(),
            text: "body text".into(),
            allow_discord_markdown: false,
            image_links: vec!["https://img.example/p.jpg".into()],
            url: "https://www.facebook.com/x".into(),
            date: -1,
            likes: "null".into(),
            comments: "null".into(),
            shares: "null".into(),
            video_links: Vec::new(),
            thumbnail: None,
        }
    }

    #[test]
    fn full_embed_emits_image_and_escapes_attribute() {
        let html = format_full_post_embed(&sample_post(), 0);
        assert!(html.contains(
            r#"<meta property="og:image" content="https://img.example/p.jpg"/>"#
        ));
        // The double-quote in the author must be HTML-escaped, never break out
        // of the content="..." attribute.
        assert!(html.contains("Title &quot;quote&quot;"));
        assert!(!html.contains(r#"content="Title "quote""#));
    }

    #[test]
    fn reel_embed_emits_video_player_card() {
        let mut post = sample_post();
        post.image_links.clear();
        post.video_links = vec!["https://video.fbcdn.net/v.mp4".into()];
        let html = format_reel_post_embed(&post, 0);
        assert!(html.contains(
            r#"<meta property="og:video" content="https://video.fbcdn.net/v.mp4"/>"#
        ));
        assert!(html.contains(r#"<meta name="twitter:card" content="player"/>"#));
    }
```

**Verify**: `cargo test --locked embed` → these pass alongside the existing
`format_description_text` tests.

### Step 3: Finder tests for the three untested parsers

Add a `#[cfg(test)] mod tests` to each file (none exist today). Use
`use serde_json::json;` and `use super::{...};`.

`src/parsers/single_photo.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::{get_content_node, get_single_image};
    use serde_json::json;

    #[test]
    fn finds_content_node_and_single_image() {
        let blocks = vec![json!({
            "message_preferred_body": {},
            "container_story": {},
            "data": {"owner": {"name": "Photog"}},
            "prefetch_uris_v2": [{"uri": "https://img.example/single.jpg"}]
        })];
        assert!(get_content_node(&blocks).is_some());
        assert_eq!(
            get_single_image(&blocks).as_deref(),
            Some("https://img.example/single.jpg")
        );
    }
}
```

`src/parsers/photocom.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::{get_attached_image_and_url, get_reaction_count};
    use serde_json::json;

    #[test]
    fn finds_reaction_count_and_attached_image() {
        let blocks = vec![json!({
            "attached_comment": {},
            "unified_reactors": {"count": 5},
            "currMedia": {
                "image": {"uri": "https://img.example/comment.jpg"},
                "attached_comment": {"feedback": {"url": "https://www.facebook.com/c"}}
            }
        })];
        assert_eq!(get_reaction_count(&blocks), Some(5));
        assert_eq!(
            get_attached_image_and_url(&blocks),
            Some((
                "https://img.example/comment.jpg".to_string(),
                "https://www.facebook.com/c".to_string()
            ))
        );
    }
}
```

`src/parsers/stories.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::find_story_bucket_and_node;
    use serde_json::json;

    #[test]
    fn finds_bucket_and_story_node() {
        let blocks = vec![json!({
            "owner": {"id": "9", "name": "Story Owner"},
            "unified_stories_with_notes": {
                "edges": [{"node": {
                    "creation_time": 123,
                    "attachments": [{"media": {"image": {"uri": "https://img.example/s.jpg"}}}]
                }}]
            }
        })];
        let (bucket, node) = find_story_bucket_and_node(&blocks).unwrap();
        assert_eq!(bucket.pointer("/owner/name").and_then(|v| v.as_str()), Some("Story Owner"));
        assert_eq!(node.get("creation_time").and_then(|v| v.as_i64()), Some(123));
    }
}
```

**Verify**: `cargo test --locked` → all three new modules pass.

### Step 4: Format

Run `cargo fmt`, then confirm clean.

**Verify**: `cargo fmt --check` → exit 0.

## Test plan

- `util`: `Story::from_json` happy path (author/id/text/url/image), progressive
  video extraction, and `attached_story` recursion via `get_text`.
- `embed`: `format_full_post_embed` emits the `og:image` and HTML-escapes a
  double-quote in the author (regression guard for attribute-injection); 
  `format_reel_post_embed` emits `og:video` + `twitter:card=player`.
- `single_photo` / `photocom` / `stories`: each parser's content-node finder
  returns the expected node from a synthetic block.
- These follow the `serde_json::json!` pattern already used in
  `src/parsers/reels.rs` and `src/parsers/video_watch.rs`.
- Verification: `cargo test --locked` → all pass; new tests are additive.

## Done criteria

ALL must hold:

- [ ] `cargo build` exits 0.
- [ ] `cargo test --locked` exits 0; the new tests in all five files run and pass.
- [ ] `cargo fmt --check` exits 0.
- [ ] `grep -c "cfg(test)" src/parsers/single_photo.rs src/parsers/photocom.rs src/parsers/stories.rs`
      shows `1` for each (each gained a test module).
- [ ] `git status` shows only the five in-scope files modified, and `git diff`
      shows only additions inside `#[cfg(test)]` blocks (no production-code lines
      changed).
- [ ] `plans/README.md` status row for 006 updated.

## STOP conditions

Stop and report back (do not improvise) if:

- The drift check shows the non-test code of any in-scope file differs from this
  plan's described shapes (e.g. a finder's matching keys changed).
- Any test you transcribe **fails** — that means the synthetic fixture doesn't
  match the code's actual behavior, or the code has a bug. Do NOT change
  production code to make the test pass and do NOT loosen the assertion to
  green-wash it; report the mismatch with the failing assertion so it can be
  triaged (a failing characterization test is a finding, not a chore).
- Making a private finder testable would require changing its visibility or
  signature — it should not (same-module `mod tests` can call private fns).

## Maintenance notes

- These are *characterization* tests: they pin current behavior so the
  behavior-changing plans (001/002/003) can't silently alter the embed output. If
  a future change intentionally changes extraction or render output, update the
  expected values in lockstep and call it out in review.
- The fixtures are synthetic and minimal — they cover the *shapes* the code
  branches on, not real Facebook payload variety. A natural follow-up is to add
  one sanitized real-HTML golden fixture per parser (the `AGENTS.md` "golden HTML
  fixtures (TODO)") run through the full fetch-less parse path; that needs a small
  test seam to build a `FetchedPage` from a string, which is out of scope here.
- If plan 002 (sticker filter) has not yet landed, the `story_extracts_author_text_and_photo`
  fixture uses a non-sticker `__typename`, so it passes either way; it does not
  depend on 002.
