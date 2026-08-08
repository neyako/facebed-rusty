# Facebed Author Identity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render a real author avatar and emit a bare author handle for every Facebed-supported Discord embed type.

**Architecture:** Carry selected owner identity through `ParsedPost`, serialize it with fxTikTok-compatible bare Activity account fields, and advertise Activity JSON from every supported HTML render path. Reuse the selected parser owner node and the existing profile-handle resolver; use direct fresh Facebook CDN avatar URLs with the Facebed logo fallback.

**Tech Stack:** Rust, axum, serde_json, reqwest, Discord Activity rendering.

## Global Constraints

- Preserve canonical post/comment URLs as account click targets.
- Never append a platform domain to `account.acct`.
- Preserve gallery order, Markdown, thread context, counters, and video-size fallbacks.
- No new dependency or open avatar proxy.
- Red test before each production behavior change.

---

### Task 1: Shared author identity model

**Files:**
- Modify: `src/parsers/mod.rs`
- Modify: `src/parsers/util.rs`
- Test: `src/parsers/util.rs`

**Interfaces:**
- Produces: `ParsedPost.author_id: Option<String>` and `ParsedPost.author_avatar_url: Option<String>`.
- Produces: `author_id_in_node(&Value)`, `author_handle_in_node(&Value)`, and `author_avatar_in_node(&Value)` helpers scoped to a selected actor/owner node.

- [ ] Add failing fixtures for `profile_picture.uri`, `profile_picture_depth_0.uri`, string `profile_pic_url`, profile URL handles, and missing data.
- [ ] Run focused parser utility tests and confirm missing fields fail compilation/assertions.
- [ ] Add the two `ParsedPost` fields and minimal identity extraction helpers.
- [ ] Update banned/test fixtures with `None` fallbacks.
- [ ] Re-run focused tests.

### Task 2: Populate identity in every parser

**Files:**
- Modify/test: `src/parsers/json_post.rs`
- Modify/test: `src/parsers/comment.rs`
- Modify/test: `src/parsers/photocom.rs`
- Modify/test: `src/parsers/reels.rs`
- Modify/test: `src/parsers/single_photo.rs`
- Modify/test: `src/parsers/stories.rs`
- Modify/test: `src/parsers/video_watch.rs`

**Interfaces:**
- Consumes: the shared identity helpers and existing `Fetcher::resolve_profile_handle`.
- Produces: every successful `ParsedPost` carries selected-owner ID/avatar and a handle when embedded or resolvable.

- [ ] Add or extend one selected-owner fixture per parser, asserting ID and avatar are taken from that owner rather than an unrelated block.
- [ ] Run the focused parser tests and confirm red failures.
- [ ] Populate identity fields from each parser's existing actor/owner node.
- [ ] Preserve Instagram reel usernames directly; resolve Facebook numeric IDs only when no embedded handle exists.
- [ ] Re-run all parser tests.

### Task 3: Bare Activity account and video attachments

**Files:**
- Modify: `src/activity.rs`
- Test: `src/activity_test.rs`

**Interfaces:**
- Consumes: `ParsedPost.author_id`, `author_handle`, `author_avatar_url`, images, video, and thumbnail.
- Produces: Discord-compatible Activity account JSON and image/video attachments.

- [ ] Change the existing account test expectation from `example.author@fb.com` to bare `example.author`; assert `id`, `username`, and `acct` are stable and hostless.
- [ ] Add failing real-avatar and Facebed-logo-fallback tests.
- [ ] Add a failing video-only attachment test matching fxTikTok's `type: video`, `url`, and `preview_url` shape.
- [ ] Implement bare account keys and avatar fallback.
- [ ] Serialize images when present; otherwise serialize the first video with thumbnail preview.
- [ ] Run focused Activity tests.

### Task 4: Advertise Activity for every supported renderer

**Files:**
- Modify/test: `src/routes.rs`
- Modify/test: `src/embed.rs`

**Interfaces:**
- Produces: Activity cache entries and alternate links for JsonPost, SinglePhoto, Photocom, Reels, Watch, Stories, and Comment.

- [ ] Replace the eligibility test with a table asserting every supported kind is eligible for a valid Facebook canonical URL, including video posts.
- [ ] Add failing HTML tests for alternate Activity links in reel and oversized-video renderers.
- [ ] Make eligibility depend on status-ID viability rather than parser kind/media type.
- [ ] Pass `activity_origin` through full, reel, and oversized-video HTML renderers.
- [ ] Run focused route/embed tests.

### Task 5: Full Rust verification

**Files:** all changed Rust files.

- [ ] Run `cargo fmt --all -- --check`.
- [ ] Run `cargo test --locked`.
- [ ] Run `cargo check --release --locked`.
- [ ] Run strict Clippy with the repository's existing allowances.
- [ ] Inspect `git diff --check`, diff stat, and full diff; preserve unrelated `.omo/` and existing redesign docs.

### Task 6: Production deployment

**Files:** deployment artifact only.

- [ ] Build Docker image for `linux/amd64` with `BUILDPLATFORM=linux/amd64`.
- [ ] Extract binary; verify x86-64 static PIE and SHA-256.
- [ ] Upload candidate to `facebed-us`, retain current binary as explicit rollback, atomically promote, and restart `facebed.service`.
- [ ] Verify active service, new PID/hash, HTTP 200, and live Activity JSON account/media fields.

### Task 7: Actual Discord display matrix

**Surface:** existing Chrome session, Discord `#test-webhook` channel.

- [ ] Obtain one current working URL for each parser kind from production logs/current Facebook session.
- [ ] Post fresh cache-busted links for default/group post, single photo, photo comment, reel, watch video, story, and comment.
- [ ] For every embed, visually verify: real selected-author avatar when exposed, Facebed/Graph fallback only when absent, bare `username`/`acct` in Activity JSON, correct content/media, one footer counter row, and working thread context where applicable.
- [ ] Record Discord's rendered handle separately from Facebed's JSON contract. Controlled bare-account, `bot: false`, and author-profile-URL comparisons all retained Discord's `@www.facebook.com` suffix, unlike fxTikTok's TikTok render.
- [ ] If a type fails, return to the relevant parser/Activity task and repeat deployment plus the full matrix.

### Task 8: Signed landing commit

**Files:** only requested code/tests and these author-identity docs.

- [ ] Stage only owned files and inspect staged diff.
- [ ] Create a signed imperative-style commit with the local GitHub SSH signing key.
- [ ] Push branch `rust` to `git@github.com:neyako/facebed-rusty.git` without the 1Password agent.
- [ ] Verify GitHub `refs/heads/rust` matches the local commit and report remaining unrelated files.
