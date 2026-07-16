# Video comment embed + page-video fix — design

Date: 2026-07-16
Branch: `video-comment-embed` (off `rust`)

## Goal

Two workstreams:

1. **Fix plain page-video links.** `https://www.facebook.com/share/v/1HJXiXrBGD/`
   (resolves to `/61576847095159/videos/1589861246186915/`) currently returns an
   error embed. Routed via `RE_PAGE_VIDEO` → `VideoWatchParser`, which fails at one
   of its probe stages (`cn` / `vn` / `opn`).
2. **Comment permalink embed.** A Facebook URL carrying `?comment_id=<id>` should
   embed the *comment's* content — text, author, date, reactions, and any attached
   image or video — styled like a normal post embed with a `(💬)` author suffix.
   Covers comments on videos, reels, and regular posts. Comments containing video
   attachments must play (video-in-comment).

## Non-goals

- Listing top comments under a video embed.
- Paginating / fetching comments not present in the server-rendered HTML.
- Any embed.rs changes — existing full/reel cards suffice.

## Workstream 1: page-video fix (recon-driven)

Cannot be specced ahead of data. Process:

1. Capture failing HTML: fetch `/61576847095159/videos/1589861246186915/` through
   the instance's `Fetcher` (with cookies), save raw HTML + extracted JSON blocks.
2. Identify which `VideoWatchParser` stage fails: content node (`cn`), video link
   (`vn`), or owner name (`opn`).
3. Add the missing key probe following AGENTS.md guidance: schema variants go in
   `parsers/util.rs::video_link_in_node` (used by every parser) or in the
   parser-local content-node matcher. Never hardcode paths — search by key via `jq`.
4. Add a unit test with a minimal JSON fixture reproducing the new shape.

## Workstream 2: `CommentParser`

### Routing (`routes.rs`)

- New `ParserKind::Comment`.
- In `catch_all`, after share-resolve + `clean_path` + the `/videos/<id>` rewrite:
  if the working URL's query contains `comment_id`, dispatch `ParserKind::Comment`.
  (Existing `?type=3` photocom check stays first — it already handles
  image-in-comment pages and runs before the crawler gate.)
- **Bug fix required:** `RE_VIDEOS` rewrite (`routes.rs:422`) rebuilds the path as
  `format!("reel/{id}")`, dropping the query string. Preserve the query so
  `comment_id` survives on bare `videos/<id>?comment_id=...` paths.
- `comment_id` is not in `url_clean::DROP_KEYS` — survives cleaning. Keep it that way.

### Parser (`src/parsers/comment.rs`)

New file, standard `Parser` trait impl, declared in `parsers/mod.rs`.

1. `ctx.fetcher.fetch(post_path, true)` → JSON blocks via `get_json_blocks`.
2. Locate the comment node matching the requested `comment_id`. FB comment ids in
   JSON are base64 of `comment:<post>_<comment_id>` (recon will confirm exact
   shape); match by decoding candidate `id` fields and/or comparing
   `legacy_fbid`-style fields. Search by key with `jq::first/all/has`, never fixed
   paths.
3. Extract from the comment node:
   - text: `preferred_body.text` (photocom precedent)
   - author: `author.name` → rendered as `"{name} (💬)"`
   - date: `created_time`
   - reactions: comment reaction count (`unified_reactors`-style key, per recon)
   - permalink URL: comment `feedback.url` if present, else the request URL
   - media: walk comment attachments; image via `image.uri`-style probe, video via
     existing `util::video_link_in_node` (handles modern/legacy/direct schemas).
     Video attachment also yields a thumbnail via `util::thumbnail_in_node`.
4. Build `ParsedPost`. `video_links` non-empty → routes render picks the reel card
   automatically; image-only → full card; text-only → full card, no media.

### Fallback

Server-rendered HTML only includes the highlighted comment for permalink-style
links; deep replies or stale ids may be absent. When `CommentParser` cannot find
the comment it returns `FacebedError::NoData`. `routes.rs` then strips
`comment_id` from the working URL and re-dispatches through the normal kind
selection, so the user gets the underlying post/video embed instead of error `C`.
Genuine parse bugs (comment found but extraction breaks) raise `Parse` with
attached HTML as usual, so they hit the Discord webhook for triage.

### Error tags

Short grep-friendly tags per repo convention: `(ccn)` comment node not found
(→ NoData/fallback), `(cau)` author missing, `(cme)` media extraction failure.

## Testing

- Unit tests with minimal JSON fixtures per repo pattern (see photocom/video_watch
  tests): comment-node matching, text/author extraction, video-attachment
  extraction, fallback trigger.
- `RE_VIDEOS` query-preservation test in routes.
- Manual verify: `curl -A 'Discordbot/2.0' localhost:9812/<path>` against a real
  video comment permalink, the example share/v link, and a text-only comment.

## Risks

- FB JSON shape for highlighted comments is recon-dependent; step 2 of each
  workstream starts with capturing real HTML before writing extraction code.
- Comment ids in URLs vs JSON may need base64 handling; confirmed during recon.
