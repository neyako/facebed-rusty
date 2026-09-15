# Plan 002: Fix the dead Sticker filter so sticker attachments stop leaking into embeds

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat 3b6521c..HEAD -- src/parsers/util.rs`
> If the file changed since this plan was written, compare the "Current state"
> excerpt below against the live code before proceeding; on a mismatch, treat
> it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug (embed integrity)
- **Planned at**: commit `3b6521c`, 2026-06-17

## Why this matters

`images_from_post` is meant to skip attachments whose media is a **Sticker**
(reaction stickers, etc.) so they are not chosen as the post's `og:image`. The
guard that does this was ported verbatim from the original Python and compares
the attachment against the Python-`repr` string `'__typename': 'Sticker'`
(single quotes, space after the colon). But the value being compared is
`serde_json::Value::to_string()`, which emits compact JSON —
`"__typename":"Sticker"` (double quotes, no space). The two never match, so the
`continue` never runs: the guard is dead code. When a sticker attachment carries
a `photo_image`/`uri`, that sticker image can be returned as the post's photo,
producing a wrong embed (a reaction sticker shown as the post's image). This fix
restores the intended skip using a real JSON check, and adds the first unit test
for `images_from_post`.

## Current state

File:

- `src/parsers/util.rs` — `images_from_post` (lines 134-205). It has **no**
  `#[cfg(test)] mod tests` block today; this plan adds one.

The broken guard, `src/parsers/util.rs:177-192`:

```rust
    // single-set: attachment with "media" but not a Sticker
    for attachment_set in &all_attachments {
        if attachment_set.get("media").is_some() {
            let dumped = attachment_set.to_string();
            if dumped.contains("'__typename': 'Sticker'") {
                continue;
            }
            let imgs: Vec<String> = jq::all(attachment_set, "photo_image")
                .into_iter()
                .filter_map(|v| val_str_at(v, "uri").map(str::to_owned))
                .collect();
            if !imgs.is_empty() {
                return imgs;
            }
        }
    }
```

`attachment_set.to_string()` is `serde_json::Value`'s `Display`, which produces
JSON like `{"media":{"__typename":"Sticker", ...}}`. The literal
`'__typename': 'Sticker'` cannot appear in that output, so the `if` is always
false. Confirm the bug is real:
`grep -n "'__typename': 'Sticker'" src/parsers/util.rs` → one match (the dead check).

### Relevant helpers already in this file

- `jq::all(root, key) -> Vec<&Value>` (`src/jq.rs:50`) — collects every value
  under `key` anywhere in the subtree. Used throughout this file.
- `val_str_at(v, key) -> Option<&str>` (`src/parsers/util.rs:46`).

### Repo conventions to follow

- "Robust JSON extraction: never index FB JSON by hardcoded path. Use
  `jq::first/all/has` to search by key." (from `AGENTS.md`). The fix uses
  `jq::all(..., "__typename")`, consistent with that rule and with how the rest
  of this function already searches by key.
- Tests live in a `#[cfg(test)] mod tests` at the bottom of the file. See the
  pattern in `src/parsers/reels.rs:334` (uses `serde_json::json!`).
- Hard gates: `cargo fmt --check` and `cargo test`.

## Commands you will need

| Purpose      | Command                       | Expected on success    |
|--------------|-------------------------------|------------------------|
| Build        | `cargo build`                 | exit 0                 |
| Tests        | `cargo test --locked`         | all pass               |
| Format       | `cargo fmt`                   | reformats in place     |
| Format check | `cargo fmt --check`           | exit 0                 |

(Exact CI gates from `.github/workflows/docker-build.yml`.)

## Scope

**In scope** (the only file you may modify):
- `src/parsers/util.rs` — fix the guard in `images_from_post`; add a `tests`
  module with two tests.

**Out of scope** (do NOT touch):
- The multi-image branch (util.rs:138-175) and the
  `comet_photo_attachment_resolution_renderer` fallback (util.rs:195-203) — both
  are unrelated to the sticker bug; leave them exactly as-is.
- `videos_from_post`, `video_link_in_node`, `thumbnail_in_node`, `Story`, and
  everything else in this file.
- Any other parser file.

## Git workflow

- Branch: `advisor/002-sticker-filter`.
- Commit style: short imperative subject (match `git log --oneline`, e.g.
  "Fix dead sticker filter in image extraction").
- Do NOT push or open a PR unless the operator instructs it.

## Steps

### Step 1: Replace the dead string check with a real JSON check

In `src/parsers/util.rs`, change the guard inside the "single-set" loop
(lines 177-192) to detect a Sticker via the parsed JSON instead of a Python-repr
string. Replace:

```rust
            let dumped = attachment_set.to_string();
            if dumped.contains("'__typename': 'Sticker'") {
                continue;
            }
```

with:

```rust
            let is_sticker = jq::all(attachment_set, "__typename")
                .into_iter()
                .any(|v| v.as_str() == Some("Sticker"));
            if is_sticker {
                continue;
            }
```

Leave the rest of the loop (the `photo_image` collection and early `return`)
unchanged.

**Verify**: `cargo build` → exit 0.

### Step 2: Add unit tests for `images_from_post`

`src/parsers/util.rs` currently has no test module. Add one at the end of the
file:

```rust
#[cfg(test)]
mod tests {
    use super::images_from_post;
    use serde_json::json;

    #[test]
    fn skips_sticker_attachment() {
        // A single-media attachment whose media is a Sticker, but which still
        // carries a photo_image.uri. The sticker image must NOT be returned.
        let post = json!({
            "attachment": {
                "media": { "__typename": "Sticker" },
                "photo_image": { "uri": "https://sticker.example/sticker.png" }
            }
        });
        assert!(images_from_post(&post).is_empty());
    }

    #[test]
    fn returns_photo_image_for_non_sticker_media() {
        let post = json!({
            "attachment": {
                "media": { "__typename": "Photo" },
                "photo_image": { "uri": "https://img.example/photo.jpg" }
            }
        });
        assert_eq!(
            images_from_post(&post),
            vec!["https://img.example/photo.jpg".to_string()]
        );
    }
}
```

**Verify**: `cargo test --locked util` → both tests pass. Before Step 1's fix,
`skips_sticker_attachment` would have failed (returning the sticker uri); after
the fix it passes.

### Step 3: Format

Run `cargo fmt`, then confirm clean.

**Verify**: `cargo fmt --check` → exit 0.

## Test plan

- `skips_sticker_attachment` — a sticker-media attachment carrying a
  `photo_image.uri`; asserts `images_from_post` returns empty. This is the direct
  regression test for the bug.
- `returns_photo_image_for_non_sticker_media` — a normal photo attachment;
  asserts the uri is returned, proving the fix didn't over-filter.
- Structural pattern: the `#[cfg(test)] mod tests` in `src/parsers/reels.rs`
  (same `use serde_json::json;` idiom).
- Verification: `cargo test --locked` → all pass, including the two new tests.

## Done criteria

ALL must hold:

- [ ] `cargo build` exits 0.
- [ ] `cargo test --locked` exits 0; the two new `util` tests pass.
- [ ] `cargo fmt --check` exits 0.
- [ ] `grep -n "'__typename': 'Sticker'" src/parsers/util.rs` returns no matches.
- [ ] `git status` shows only `src/parsers/util.rs` modified.
- [ ] `plans/README.md` status row for 002 updated.

## STOP conditions

Stop and report back (do not improvise) if:

- The drift check shows `src/parsers/util.rs` changed since `3b6521c` and the
  "Current state" excerpt no longer matches.
- `skips_sticker_attachment` still fails after Step 1 — that means a sticker's
  `__typename` is not `"Sticker"` in the JSON shape you're testing; report what
  you find rather than broadening the match (do not start matching substrings of
  `__typename`, which could over-filter real photos).
- The fix appears to require touching the multi-image branch or any other file.

## Maintenance notes

- The fix treats an attachment as a sticker if *any* `__typename` in its subtree
  equals `"Sticker"`. That mirrors the original intent (skip sticker media) and
  is safe for the single-media branch, where the attachment subtree is small. If
  a future change reuses this guard somewhere a `"Sticker"` typename can legitimately
  co-occur with a wanted photo in the same subtree, narrow it to the media node's
  own `__typename`.
- Reviewer should confirm no real photo posts regress to empty images — the
  second test guards that, but a spot-check against a known multi-photo post is
  worthwhile.
