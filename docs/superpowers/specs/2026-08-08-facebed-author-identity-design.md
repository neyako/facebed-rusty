# Facebed Author Identity Design

## Goal

Every supported Facebook embed type renders through Discord Activity with the real author identity: Facebed emits a bare handle and the author's profile photo. Discord may still decorate the rendered handle with Facebook's host; controlled comparisons show this is platform behavior rather than a domain embedded in Facebed's Activity JSON.

## Supported types

- Default/group/text/image/shared posts (`JsonPost`)
- Single-photo posts (`SinglePhoto`)
- Photo comments (`Photocom`)
- Reels (`Reels`)
- Watch/video posts (`Watch`)
- Stories (`Stories`)
- Text/image/video comments (`Comment`)

## Activity account contract

- `account.id`: stable author ID, then handle, then `facebed` fallback.
- `account.username`: bare author handle, then author ID, then `facebed` fallback.
- `account.acct`: exactly the same bare value as `username`; never append `@fb.com`, `@facebook.com`, or another host.
- `account.display_name`: parsed Facebook author name.
- `account.url`: canonical post/comment URL, preserving the existing post-link behavior.
- `account.avatar` and `avatar_static`: parsed author profile photo URL when present; otherwise Facebook's Graph profile-picture URL for a known handle, then `https://facebed.neyahub.com/favicon.ico`.
- `account.header` and `header_static`: keep `https://facebed.neyahub.com/banner.png`.

This matches fxTikTok's Activity account shape: `username` and `acct` are the bare creator ID, while `avatar` points to the creator photo. fxTikTok also uses the profile URL and `bot: false`; neither change removes Discord's Facebook suffix when applied to Facebed, indicating Discord handles TikTok specially.

## Parser data flow

Add `author_id: Option<String>` and `author_avatar_url: Option<String>` to `ParsedPost`. A shared parser helper extracts identity from the parser's already-selected actor/owner node, avoiding whole-document searches that could select a sidebar or suggested account.

Recognized avatar shapes:

- `profile_picture.uri`
- `profile_picture_depth_0.uri`
- `profile_picture_depth_1.uri`
- string/object forms of `profile_pic_url`, `profile_pic_url_hd`, and `profilePictureUrl`

Recognized embedded handle sources remain profile `url` fields and Instagram `username`. Facebook parsers may use the existing numeric-ID profile redirect lookup when the selected owner has no vanity handle.

## Media and Activity eligibility

All supported parser kinds advertise `application/activity+json` when the canonical URL can produce a Facebed status ID.

- Image or gallery posts: first four image attachments, unchanged.
- Video-only posts: one `type: video` attachment using the Facebook video URL and parsed thumbnail as `preview_url`.
- Mixed image/video posts: retain the existing image-first behavior so adding author identity does not hide galleries.
- Text-only and comment posts: no media attachment.
- Shared/comment context: retain the existing Activity blockquote/thread content.

Reel, watch, story, comment, and oversized-video HTML paths must emit the same alternate Activity link as full posts.

## Fallbacks and safety

- Missing author ID or handle never prevents an embed; use `facebed` as the bare account key.
- Missing or malformed embedded avatar never prevents an embed; try the Graph profile-picture URL for a known handle, then use the Facebed logo.
- Do not create an open avatar-proxy endpoint or accept arbitrary remote URLs, avoiding an SSRF surface.
- Do not change Facebook post/profile click targets, reaction counters, Markdown behavior, or gallery ordering.

## Verification

Automated verification covers bare `acct`, real-avatar/fallback selection, video Activity attachments, all parser kinds being Activity eligible, alternate links on video/comment paths, and parser identity extraction fixtures. Final QA deploys the amd64 binary and posts fresh cache-busted links for every supported type in the existing Discord `#test-webhook` channel, visually checking author avatar, content, media, and footer regressions. The API assertion, not Discord's decorated label, proves Facebed emits no platform domain.
