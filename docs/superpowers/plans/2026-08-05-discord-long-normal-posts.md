# Discord Long Normal Posts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Discord consume complete Facebed captions through a Mastodon-compatible status endpoint for full posts with one image, multiple images, or no images.

**Architecture:** A new pure `activity` module encodes Facebook URLs into digits-only status IDs, emits the alternate link, and builds Mastodon-shaped JSON from `ParsedPost`. Full embed rendering advertises the link; a dedicated API route serves cached parsed posts and performs one existing-parser fetch on cache miss.

**Tech Stack:** Rust 2021, axum 0.7, serde/serde_json, chrono, html-escape, existing `ParsedPost` and `EmbedCache`.

## Global Constraints

- Preserve the existing modified `src/parsers/single_photo.rs` and untracked `benchmarks/` exactly except for verification reads.
- Do not change `JsonPostParser`, `Story`, reel/watch/story/comment/video formatters, or the 4,096-character Open Graph cap.
- Advertise Activity only for video-free `JsonPost` and `SinglePhoto` results; exclude mixed-video posts and every other parser kind.
- Status IDs decode to at most 2,048 bytes and must select an absolute Facebook page URL.
- Activity responses expose at most four image attachments in source order and an empty array for text-only posts.
- Every production behavior starts with a test observed failing for the intended reason.
- No new dependency, commit, push, deployment, or production restart.

---

### Task 1: Pure Activity Compatibility Module

**Files:**
- Create: `src/activity.rs`
- Modify: `src/main.rs` near the module declarations

**Interfaces:**
- Consumes: `crate::parsers::ParsedPost`, `crate::url_clean`, existing `chrono`, `html_escape`, and `serde_json` dependencies.
- Produces:
  - `pub fn status_id(post_url: &str) -> Option<String>`
  - `pub fn decode_status_path(id: &str) -> Option<String>`
  - `pub fn alternate_link(post_url: &str) -> String`
  - `pub fn status_json(id: &str, post: &ParsedPost) -> String`

- [x] **Step 1: Add the module declaration and failing ID tests**

Add `mod activity;` in `src/main.rs`. Create `src/activity.rs` with tests that call the four not-yet-defined functions. The ID fixture must be `https://www.facebook.com/groups/example/posts/123?comment_id=9`; assert digits only and exact decoded clean path. Add rejection cases for `"12x"`, `"999"`, a length not divisible by three, an encoded `https://example.com/x`, and a decoded payload over 2,048 bytes.

- [x] **Step 2: Run the ID test and verify RED**

Run: `cargo test activity::tests::status_id_round_trips_facebook_path --locked`

Expected: compilation fails because the activity functions do not exist.

- [x] **Step 3: Implement bounded numeric encoding and alternate link**

Use exactly three decimal digits per UTF-8 byte. Reject non-Facebook URLs before encoding and all malformed or oversized inputs while decoding. `alternate_link` must render:

```html
<link href="/users/facebed/statuses/{digits}" rel="alternate" type="application/activity+json"/>
```

Return an empty string if `post_url` is not an allowed Facebook page URL.

- [x] **Step 4: Run ID tests and verify GREEN**

Run: `cargo test activity::tests::status_id --locked`

Expected: all ID and link tests pass.

- [x] **Step 5: Add failing text-only and three-image status tests**

Build real `ParsedPost` fixtures. The text-only fixture must exceed 1,024 Unicode characters, end with `FINAL_SENTINEL`, contain `<unsafe>&`, and contain a newline. Parse `status_json` with `serde_json::from_str::<Value>` and assert:

```rust
assert!(json["content"].as_str().unwrap().contains("FINAL_SENTINEL"));
assert!(json["content"].as_str().unwrap().contains("&lt;unsafe&gt;&amp;"));
assert!(json["content"].as_str().unwrap().contains("<br>"));
assert_eq!(json["media_attachments"].as_array().unwrap().len(), 0);
```

The gallery fixture must have three distinct image URLs; assert length three, `type == "image"`, and source order.

- [x] **Step 6: Run status tests and verify RED**

Run: `cargo test activity::tests::status_json --locked`

Expected: compilation fails because `status_json` is missing.

- [x] **Step 7: Implement the Mastodon-shaped status document**

Build JSON with `serde_json::json!`. Required fields are `id`, `url`, `uri`, `created_at`, null edit/reblog/reply values, `content`, `spoiler_text`, `visibility`, `application`, `account`, `media_attachments`, empty mentions/tags/emojis, and null card/poll. Escape caption text with `html_escape::encode_text`, replace newlines with `<br>`, append non-null reaction counts, and include at most four image attachments.

- [x] **Step 8: Run all activity tests and verify GREEN**

Run: `cargo test activity::tests --locked`

Expected: all activity tests pass.

### Task 2: Short-Lived Parsed-Post Activity Cache

**Files:**
- Modify: `src/embed_cache.rs`

**Interfaces:**
- Consumes: `crate::parsers::ParsedPost`, `EMBED_CACHE_TTL`, `EMBED_CACHE_MAX`.
- Produces:
  - `pub fn insert_activity(&mut self, id: &str, post: ParsedPost, now: Instant)`
  - `pub fn get_activity(&mut self, id: &str, now: Instant) -> Option<ParsedPost>`

- [x] **Step 1: Write failing expiry and bound tests**

Extend the cache test with a `ParsedPost` fixture. Assert a just-inserted activity post is returned, the same entry returns `None` after `EMBED_CACHE_TTL`, and inserting `EMBED_CACHE_MAX + 1` unique IDs keeps the activity map at or below the bound.

- [x] **Step 2: Run the cache test and verify RED**

Run: `cargo test embed_cache::tests::activity_cache_expires_and_bounds_entries --locked`

Expected: compilation fails because the activity methods/map do not exist.

- [x] **Step 3: Implement the second bounded map**

Add a private activity entry containing `ParsedPost` and `stored_at`. Mirror the existing TTL removal and oldest-entry eviction behavior without changing the rendered-body map contract.

- [x] **Step 4: Run cache tests and verify GREEN**

Run: `cargo test embed_cache::tests --locked`

Expected: rendered-body and activity-cache tests pass.

### Task 3: Advertise and Serve Activity Statuses

**Files:**
- Modify: `src/embed.rs` in `format_full_post_embed` and its tests
- Modify: `src/routes.rs` in route registration, the success-cache seam, and a focused activity handler

**Interfaces:**
- Consumes: Task 1 activity functions and Task 2 activity-cache methods.
- Produces: `GET /api/v1/statuses/:id` with `application/json; charset=utf-8`.
- Produces: `fn activity_path(id: &str) -> Result<(String, ParserKind), StatusCode>` for boundary classification.
- Changes: `format_full_post_embed(post: &ParsedPost, tz_offset: i32, activity_enabled: bool) -> String`.

- [x] **Step 1: Write the failing full-embed link test**

Add `full_embed_advertises_activity_status` beside the existing oEmbed test. Call the formatter with `activity_enabled=true`, compute `crate::activity::status_id(&post.url)`, and assert the generated full HTML contains `/users/facebed/statuses/{id}` plus `type="application/activity+json"`. Add `full_embed_omits_activity_status_when_disabled` using `false`. Existing callers/tests must pass the new flag explicitly; the reel formatter remains unchanged.

- [x] **Step 2: Run the embed test and verify RED**

Run: `cargo test embed::tests::full_embed_advertises_activity_status --locked`

Expected: assertion fails because the activity link is absent.

- [x] **Step 3: Insert the alternate link in the full formatter only**

When `activity_enabled` is true, call `crate::activity::alternate_link(&post.url)` once and place its output after the existing oEmbed link; otherwise insert an empty string. Do not modify reel or oversized-video templates.

- [x] **Step 4: Run embed tests and verify GREEN**

Run: `cargo test embed::tests --locked`

Expected: full embed contains the link; reel tests remain green.

- [x] **Step 5: Add route-level failing tests for activity-path classification**

Add `activity_path_rejects_malformed_and_unsupported_ids`. Assert `activity_path("12x") == Err(StatusCode::BAD_REQUEST)`. Encode `https://www.facebook.com/marketplace/item/123` with `status_id` and assert its decoded but unsupported path returns `Err(StatusCode::NOT_FOUND)`. Add a supported group-post case and assert it returns `ParserKind::JsonPost` with `groups/example/posts/123`. Add story/reel/comment-ID cases and assert `404`; these parser kinds are out of scope. Do not construct a fake Facebook parser.

- [x] **Step 6: Run route-focused tests and verify RED**

Run: `cargo test routes::tests::activity --locked`

Expected: compilation fails because `activity_path` does not exist.

- [x] **Step 7: Implement route, cache hit, and one-attempt cache miss**

Register `.route("/api/v1/statuses/:id", get(activity_status))` before `/*path`. Handler flow:

```text
decode ID -> get cached ParsedPost -> build JSON
          -> on miss select ParserKind, acquire fetch permit, run_parser once,
             insert activity cache, build JSON
```

Use `400` for malformed IDs, `404` for unsupported paths/parser failures/video-bearing parsed results, and `503` for semaphore exhaustion or timeout. Never pass this route through crawler gating or return an HTML error embed.

Implement `activity_path` as the single decode/normalize/`select_kind` boundary used by both the tests and handler.

During successful rendering, set `activity_enabled` only when `kind` is `JsonPost` or `SinglePhoto` and `post.video_links` is empty. Only then compute `status_id(&post.url)` and insert `post.clone()` into the activity cache before returning the HTML body.

- [x] **Step 8: Run route tests and verify GREEN**

Run: `cargo test routes::tests::activity --locked`

Expected: all activity route tests pass.

### Task 4: Verification and Real-Surface Smoke

**Files:**
- Verify only: all changed source and plan/spec files
- Preserve: `benchmarks/`

**Interfaces:**
- Consumes: completed Tasks 1-3.
- Produces: test, lint, HTTP, and cleanup evidence.

- [x] **Step 1: Format and run focused tests**

Run:

```bash
cargo fmt --all -- --check
cargo test activity::tests --locked
cargo test embed_cache::tests --locked
cargo test embed::tests --locked
cargo test routes::tests::activity --locked
```

If format check fails only because new code needs formatting, run `cargo fmt --all`, then repeat the check.

- [x] **Step 2: Run the full locked suite and lint**

Run:

```bash
cargo test --locked
cargo clippy --all-targets --all-features -- -D warnings
```

Record unrelated pre-existing Clippy failures separately; do not change unrelated files.

- [x] **Step 3: Run the programming post-write checks**

Measure every modified source file's nonblank, non-comment LOC. Confirm the new module is below 200 pure LOC, new functions take at most three inputs, no production `unwrap`/`expect` was introduced, user input is validated at the activity boundary, and each new behavior has a test that was observed red first.

- [x] **Step 4: Exercise both supplied posts over real local HTTP**

Start Facebed without cookies on port 9812 and request both supplied share paths with `Discordbot/2.0`. For each HTML response:

1. assert the known complete caption ending remains in `og:description`;
2. extract `/users/facebed/statuses/{id}`;
3. request `/api/v1/statuses/{id}`;
4. assert text-only JSON contains `Cảm ơn các CT nhé` and zero attachments;
5. assert gallery JSON contains `rất đáng cân nhắc.` and three ordered attachments.

- [x] **Step 5: Attempt a fresh public Discord render without deployment**

If an already-installed temporary tunnel and an authorized Discord test surface are available, expose the local port, use unique query paths, capture both rendered previews, then close the tunnel. Otherwise report this exact gate as requiring the maintainer to post fresh production URLs after deployment; do not deploy or transmit through an unknown webhook.

- [x] **Step 6: Clean all debug artifacts**

Stop local server/tunnel sessions, remove only this session's debug journal and its `.git/info/exclude` line, and verify `git status --short` contains only the preserved one-image fix, the approved Activity implementation/spec/plan, and user-owned `benchmarks/`.
