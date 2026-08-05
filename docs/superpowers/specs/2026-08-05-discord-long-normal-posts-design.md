# Discord Long Normal Posts Design

## Goal

Make Discord render the complete Facebed caption for Facebook posts that have multiple images or no images, matching the long-body behavior demonstrated by FixupX. Preserve the existing one-image caption fix and all existing video/reel behavior.

## Runtime evidence

The corrected production host, `facebed.neyahub.com`, already returns complete Open Graph data for both supplied examples:

- gallery: 733 Unicode characters and three `og:image` tags;
- text-only: 1,158 Unicode characters and no `og:image` tags.

Anonymous local HEAD returns the same values. Therefore neither `JsonPostParser`, `Story::from_json`, nor the 4,096-character Open Graph cap reproduces this truncation.

FixupX build `923ef3b` adds a same-origin `rel="alternate"` link with media type `application/activity+json`. Discord recognizes the Mastodon-shaped status URL and requests `/api/v1/statuses/:id`; the response carries full HTML status content and zero or more media attachments. FixupX's separate oEmbed response only supplies attribution and caps its author text, so changing Facebed's oEmbed type alone is not the long-body mechanism.

## Architecture

Add a focused `src/activity.rs` compatibility module. It owns three pure boundaries:

1. encode a canonical Facebook post URL as a reversible, digits-only status ID;
2. render the same-origin Mastodon-shaped alternate link;
3. serialize a `ParsedPost` as the public subset of a Mastodon Status entity Discord consumes.

`format_full_post_embed` accepts an explicit Activity-eligibility flag. Routing enables it only for video-free `JsonPost` and `SinglePhoto` results. Reel, watch, story, comment, image-in-comment, mixed-video, and oversized-video results remain unchanged.

Register `GET /api/v1/statuses/:id` before the catch-all route. The handler decodes and validates the ID, then reads the matching `ParsedPost` from a short-lived activity cache populated during the initial Facebed render. A cache miss decodes the canonical Facebook path and invokes the existing parser once under the existing fetch semaphore. Invalid IDs return JSON `400`; unsupported or unavailable posts return JSON `404` or `503`, never a Facebed HTML error embed.

## Status identifier

The encoder converts each UTF-8 byte of the canonical Facebook URL to exactly three decimal digits. The decoder requires:

- digits only;
- a non-empty length divisible by three;
- every chunk in `000..=255`;
- at most 2,048 decoded bytes;
- valid UTF-8;
- an absolute `facebook.com` page URL.

The decoded URL is normalized through `url_clean::clean_path` before parser dispatch. This keeps the compatibility route stateless on cache miss and prevents an encoded ID from selecting a non-Facebook upstream.

## Mastodon compatibility response

The JSON response follows the current public `GET /api/v1/statuses/:id` entity shape and the subset proven by FixupX:

- `id`, `url`, and `uri` identify the Facebook post;
- `created_at` is UTC RFC 3339, falling back to the Unix epoch for unknown dates;
- `content` contains HTML-escaped full post text with newlines converted to `<br>`;
- `account.display_name` contains the Facebook author and the remaining account fields are stable compatibility values;
- `media_attachments` contains up to four image objects in original order, or an empty array for text-only posts;
- `mentions`, `tags`, and `emojis` are empty; `card`, `poll`, `reblog`, and reply identifiers are null;
- visible reaction counts are appended to `content` only when their existing value is not `"null"`.

User-derived text and URLs are serialized through `serde_json`; post text is HTML-escaped before entering `content`. No raw Facebook HTML is forwarded.

## Cache behavior

Extend `EmbedCache` with a bounded activity map keyed by status ID. Entries store cloned `ParsedPost` values and use the existing 90-second TTL and 512-entry bound. The initial HTML render inserts the activity entry before returning. The activity handler reads it immediately, avoiding a second Facebook request in the normal Discord crawl sequence.

A missing or expired activity entry is not fatal: the handler decodes the path and runs the existing parser once. This makes restarts and delayed Discord fetches recoverable without duplicating the full account-retry state machine.

## Scope

In scope:

- video-free normal posts and single-photo posts, including one image, multiple images, and text-only posts;
- Mastodon-compatible status JSON;
- activity-cache storage and cache-miss parsing;
- focused unit tests and local HTTP smoke tests using the supplied posts.

Out of scope:

- changing Facebook caption extraction for normal posts;
- changing reel, watch, inline-video, mixed-video, story, comment, image-in-comment, or oversized-video rendering;
- image mosaics, image dimension probes, avatars, federation, WebFinger, inbox/outbox, or a general ActivityPub server;
- changing the existing 4,096-character Open Graph cap;
- committing, pushing, deploying, or restarting production.

## Tests and acceptance

Tests must be written and observed failing before production implementation.

Acceptance requires:

1. digits-only ID round-trip plus malformed, oversized, invalid UTF-8, and non-Facebook rejection;
2. a long text-only `ParsedPost` keeps its final sentinel in `content` and returns an empty media list;
3. a three-image `ParsedPost` returns three image attachments in input order;
4. HTML metacharacters and newlines in caption text cannot inject status HTML;
5. full embed HTML advertises the same status ID used by the API route;
6. activity-cache entries expire and stay within the existing bound;
7. focused tests, `cargo test --locked`, formatting, and applicable lint checks pass;
8. local Discordbot curls for both supplied links return complete OG text plus a valid activity link and matching status JSON.

Actual Discord rendering remains a deployment-surface gate: local HTTP proves the contract, while a fresh public URL must be posted in Discord to prove Discord consumes it.
