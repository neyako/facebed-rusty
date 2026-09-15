# Facebed Bold Engagement Design

## Goal

Make the existing rich/image Activity engagement row bold in Discord while preserving its current placement, order, values, and surrounding embed behavior.

## Output

Keep the row text unchanged:

```text
😂 👍 83 • 💬 109 • 🔁 0
```

Render the entire row with Activity HTML emphasis. Do not reorder counters, abbreviate counts, add view counts, or change emoji selection.

## Behavior

- Keep engagement after the focal and quoted post text, before Discord renders media attachments.
- Apply bold markup only to the engagement row, not to post text or quoted content.
- Escape the engagement string before inserting it into Activity HTML.
- Keep video-only Activity engagement omitted to prevent duplication with video oEmbed.
- Keep oEmbed engagement, `og:site_name` footer, Activity `created_at`, media ordering, author identity, and oversized-video behavior unchanged.

## Scope

The production change belongs only in Activity content rendering and its focused tests. Parser, routing, cache, cookies, reaction extraction, and footer formatting are out of scope.

## Verification

- A RED test proves the rich/image Activity engagement row is not bold before the change.
- GREEN tests require the exact existing row inside `<strong>...</strong>` and require the post body to remain outside that element.
- Existing mixed-media, quoted-content, escaping, and video-only tests remain green.
- Run formatting, strict clippy, and the full Rust test suite.
- Verify live rich/image Activity JSON contains the bold row and live video-only Activity still omits engagement.
