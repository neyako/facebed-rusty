# Plan 010: Group-post embed descriptions render clean Markdown — no leaked `#`, `\`, or dead `**`

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: `git diff --stat 3148b75..HEAD -- src/embed.rs`
> If `src/embed.rs` changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `3148b75`, 2026-06-18

## Why this matters

Group posts opt into rendering a subset of the poster's Markdown in the Discord
embed description (`allow_discord_markdown = true`, set in
`src/parsers/json_post.rs:90` for any `groups/` path). The intent is to make
group posts render *nicer* than raw Facebook text (e.g. real **bold** and
`> blockquotes`). The current implementation leaks three classes of junk into
the embed body — all visible in a single real group post:

1. **Headings show literal `#`.** A post starting `# ⚠️ Cảnh báo…` renders the
   hash sign verbatim, because `escape_markdown` has no `#` case and Discord
   embed *descriptions* do not render ATX headings.
2. **Facebook backslash-escapes double up into a visible `\`.** Source text
   containing `\*` becomes `\\\*` after escaping, which Discord renders as `\*`
   — a stray backslash in the prose (`từ: \*4 OCPU`).
3. **Bold with an inner padding space dies.** `**Oracle **` is preserved
   verbatim, but CommonMark flanking rules reject a `**` that is preceded by a
   space, so Discord shows the asterisks literally instead of bolding.

After this plan, group-post descriptions strip heading markers (keeping the
text), honor Facebook's `\`-escapes by showing the literal character, and
normalize padded bold so it actually bolds. Non-group posts
(`allow_discord_markdown == false`) are unchanged.

## Current state

- `src/embed.rs` — OpenGraph HTML output. The description formatter and its
  Markdown helpers live at lines 49–126. `format_description_text` is the only
  caller of these helpers; it is invoked by all three full-post embed builders
  (`format_full_post_embed`, `format_reel_post_embed`,
  `format_oversized_video_embed`) via
  `escape_attr(&format_description_text(truncate_chars(&post.text, 4096), post.allow_discord_markdown))`.
- `src/parsers/json_post.rs:90` — sets `allow_discord_markdown: is_group_post_path(post_path)`.
  `is_group_post_path` (lines 268–270) is true when the cleaned path starts with
  `groups/`. **Do not touch this file** — the trigger is correct; only the
  rendering is wrong.

The code to replace, exactly as it exists today (`src/embed.rs:49-126`):

```rust
/// Escape markdown control chars that Discord renders inside `og:description`.
/// Intentional FB group-post markdown should survive, but punctuation in normal
/// prose should not accidentally bold/quote/code-format the embed body.
fn escape_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '*' | '_' | '~' | '|' | '`' | '>' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

fn unescape_paired_marker(s: &str, escaped_marker: &str, raw_marker: &str) -> String {
    let positions: Vec<usize> = s.match_indices(escaped_marker).map(|(i, _)| i).collect();
    if positions.len() < 2 {
        return s.to_owned();
    }

    let paired_count = positions.len() - (positions.len() % 2);
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0;
    for (idx, pos) in positions.into_iter().enumerate() {
        out.push_str(&s[cursor..pos]);
        if idx < paired_count {
            out.push_str(raw_marker);
        } else {
            out.push_str(escaped_marker);
        }
        cursor = pos + escaped_marker.len();
    }
    out.push_str(&s[cursor..]);
    out
}

fn unescape_line_start_blockquotes(escaped: &str, raw: &str) -> String {
    let escaped_lines: Vec<&str> = escaped.split_inclusive('\n').collect();
    let raw_lines: Vec<&str> = raw.split_inclusive('\n').collect();
    if escaped_lines.len() != raw_lines.len() {
        return escaped.to_owned();
    }

    let mut out = String::with_capacity(escaped.len());
    for (escaped_line, raw_line) in escaped_lines.iter().zip(raw_lines.iter()) {
        let leading = raw_line
            .char_indices()
            .find(|(_, c)| !c.is_whitespace() || *c == '\n')
            .map(|(i, _)| i)
            .unwrap_or(raw_line.len());
        let marker = &raw_line[leading..];
        if !(marker.starts_with("> ") || marker == ">" || marker == ">\n") {
            out.push_str(escaped_line);
            continue;
        }

        if escaped_line.len() >= leading + 2 && &escaped_line[leading..leading + 2] == r"\>" {
            out.push_str(&escaped_line[..leading]);
            out.push('>');
            out.push_str(&escaped_line[leading + 2..]);
        } else {
            out.push_str(escaped_line);
        }
    }
    out
}

fn format_description_text(s: &str, allow_discord_markdown: bool) -> String {
    let escaped = escape_markdown(s);
    if !allow_discord_markdown {
        return escaped;
    }
    let with_bold = unescape_paired_marker(&escaped, r"\*\*", "**");
    unescape_line_start_blockquotes(&with_bold, s)
}
```

**Repo conventions that apply here** (from `AGENTS.md` "Patterns to follow"):
- Keep the change inside `src/embed.rs`; this is pure string formatting, no new
  deps, no framework changes.
- Tests live in a `#[cfg(test)] mod tests` at the bottom of the same file — see
  `src/embed.rs:415-505` for the existing pattern. Match it.
- Commit style: short imperative subject, no Conventional-Commits prefix, body
  only when "why" isn't obvious (see `git log --oneline`).

## Commands you will need

| Purpose   | Command                          | Expected on success            |
|-----------|----------------------------------|--------------------------------|
| Build     | `cargo build`                    | exit 0                         |
| Tests     | `cargo test`                     | all pass (84 today + new)      |
| Scoped    | `cargo test --lib embed`         | the `embed` tests pass         |
| Format    | `cargo fmt -- --check`           | exit 0, no diff                |

Baseline: `cargo test` reports **84 passed** at commit `3148b75` (per
`plans/README.md` reconcile log). Confirm this before starting.

## Scope

**In scope** (the only file you should modify):
- `src/embed.rs`

**Out of scope** (do NOT touch, even though they look related):
- `src/parsers/json_post.rs` — the `allow_discord_markdown = is_group_post_path(...)`
  trigger is correct; this plan only fixes *rendering*, not *when* it applies.
- The `escape_attr` / `quote` functions and all HTML-template builders in
  `embed.rs` — the description string flows through `escape_attr` unchanged;
  do not alter the attribute escaping.
- The `allow_discord_markdown == false` behavior — non-group posts must keep
  rendering exactly as today (every special escaped, `#` shown literally).

## Git workflow

- Branch: `advisor/010-group-post-markdown`
- One commit is fine; message style per `git log` (e.g.
  `Fix group-post markdown leaks in embed description`).
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Replace the Markdown helpers with a single-pass group-post renderer

In `src/embed.rs`, **delete** these three functions entirely:
`unescape_paired_marker`, `unescape_line_start_blockquotes`, and the body of
`format_description_text` (you will rewrite it). **Keep** `escape_markdown`
unchanged — it is still the renderer for the `allow_discord_markdown == false`
path.

Replace the deleted code with the following. This is a deliberate rewrite from
"escape-everything-then-selectively-unescape" (which cannot honor a Facebook
`\`-escape without an ordering hazard) to a single forward pass that decides
each character's fate once:

```rust
/// Markdown specials that get a literal backslash-escape so Discord renders
/// them as plain text instead of formatting.
const MD_ESCAPE: &[char] = &['*', '_', '~', '|', '`', '>'];

/// Chars a Facebook-authored `\X` escape may protect. Includes `#` and `\`
/// themselves, which `MD_ESCAPE` deliberately omits (a lone `#`/`\` is handled
/// elsewhere).
fn is_md_special(c: char) -> bool {
    MD_ESCAPE.contains(&c) || c == '#' || c == '\\'
}

fn push_escaped(out: &mut String, c: char) {
    out.push('\\');
    out.push(c);
}

fn format_description_text(s: &str, allow_discord_markdown: bool) -> String {
    if !allow_discord_markdown {
        return escape_markdown(s);
    }
    render_group_markdown(s)
}

/// Render trusted FB-group-post text for a Discord embed description.
/// FB stores plain text, but group posters often write Markdown intending
/// formatting. We render a safe subset (bold, blockquote) and neutralize the
/// rest so ordinary punctuation cannot accidentally format the embed.
fn render_group_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, line) in s.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        render_group_markdown_line(&mut out, line);
    }
    out
}

fn render_group_markdown_line(out: &mut String, line: &str) {
    // Preserve leading whitespace verbatim.
    let ws_end = line
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    out.push_str(&line[..ws_end]);
    let mut rest = &line[ws_end..];

    // Heading: strip 1..=6 leading '#' followed by a space (Discord embed
    // descriptions do not render ATX headings — keep the text, drop the marks).
    let hashes = rest.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && rest[hashes..].starts_with(' ') {
        rest = rest[hashes..].trim_start_matches(' ');
    }

    // Blockquote: a real '>' marker survives; render the remainder inline.
    if rest == ">" || rest.starts_with("> ") {
        out.push('>');
        render_inline(out, &rest[1..]);
        return;
    }

    render_inline(out, rest);
}

fn render_inline(out: &mut String, s: &str) {
    let chars: Vec<char> = s.chars().collect();

    // First pass: locate non-escaped "**" markers and decide which are paired.
    // Even-numbered markers open a span, odd-numbered close it; an unpaired
    // trailing marker is escaped (left literal).
    let mut markers: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2; // skip the escaped char so "\*" never counts toward a pair
            continue;
        }
        if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            markers.push(i);
            i += 2;
            continue;
        }
        i += 1;
    }
    let paired = markers.len() - (markers.len() % 2);
    let mut opens = std::collections::HashSet::new();
    let mut closes = std::collections::HashSet::new();
    for (n, &pos) in markers.iter().take(paired).enumerate() {
        if n % 2 == 0 {
            opens.insert(pos);
        } else {
            closes.insert(pos);
        }
    }

    // Second pass: emit.
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            // Honor a FB-authored escape: show the next char literally.
            if let Some(&next) = chars.get(i + 1) {
                if is_md_special(next) {
                    push_escaped(out, next);
                    i += 2;
                    continue;
                }
            }
            // Lone backslash -> literal backslash.
            push_escaped(out, '\\');
            i += 1;
            continue;
        }
        if c == '*' && opens.contains(&i) {
            out.push_str("**");
            i += 2;
            // Trim padding immediately after the opening marker.
            while chars.get(i) == Some(&' ') {
                i += 1;
            }
            continue;
        }
        if c == '*' && closes.contains(&i) {
            // Trim padding immediately before the closing marker.
            while out.ends_with(' ') {
                out.pop();
            }
            out.push_str("**");
            i += 2;
            continue;
        }
        if MD_ESCAPE.contains(&c) {
            push_escaped(out, c);
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
}
```

**Verify**: `cargo build` → exit 0. Then `cargo fmt -- --check` → exit 0
(if it reports a diff, run `cargo fmt` and re-check).

### Step 2: Update the one existing test whose expectation this changes

The existing test `preserves_intentional_discord_markdown` (currently
`src/embed.rs:436-444`) asserts that a leading `#` survives verbatim. Stripping
heading markers is the intended fix (M1), so this expectation changes. Update
**only** the expected string to drop the `# ` prefix on the heading line:

Current:
```rust
    #[test]
    fn preserves_intentional_discord_markdown() {
        let text = "> Dmm GenG oi\n# **FIFA WORLD CUP 2026**\n\n**Germany vs Curacao**";

        assert_eq!(
            format_description_text(text, true),
            "> Dmm GenG oi\n# **FIFA WORLD CUP 2026**\n\n**Germany vs Curacao**"
        );
    }
```

Replace the expected value (second string only) with the heading marker stripped:
```rust
    #[test]
    fn preserves_intentional_discord_markdown() {
        let text = "> Dmm GenG oi\n# **FIFA WORLD CUP 2026**\n\n**Germany vs Curacao**";

        assert_eq!(
            format_description_text(text, true),
            "> Dmm GenG oi\n**FIFA WORLD CUP 2026**\n\n**Germany vs Curacao**"
        );
    }
```

Leave the other three `format_description_text` tests
(`escapes_markdown_when_not_allowed`, `escapes_accidental_markdown_punctuation`,
`leaves_unmatched_bold_marker_escaped`) **unchanged** — they must still pass as
written. If any of them fails, that is a STOP condition (it means the rewrite
changed behavior it should not have).

**Verify**: `cargo test --lib embed` → all pass.

### Step 3: Add tests for the three fixed leaks

Add these four tests inside the existing `#[cfg(test)] mod tests` in
`src/embed.rs`, next to the other `format_description_text` tests. They pin the
three bug fixes plus the escaped-bold edge case:

```rust
    #[test]
    fn strips_leading_heading_markers() {
        // M1: '#'..'######' + space at line start are removed, text kept.
        assert_eq!(
            format_description_text("# ⚠️ Cảnh báo\n### Sub", true),
            "⚠️ Cảnh báo\nSub"
        );
        // A mid-line '#' (hashtag) is left alone.
        assert_eq!(format_description_text("a #b c", true), "a #b c");
        // Seven hashes is not a heading — escaped like ordinary text is not,
        // but '#' is not in MD_ESCAPE, so it stays literal.
        assert_eq!(format_description_text("####### x", true), "####### x");
    }

    #[test]
    fn honors_fb_backslash_escape() {
        // M2: FB source "\*4" must render a single literal '*', not "\\*".
        assert_eq!(format_description_text(r"từ: \*4 OCPU", true), r"từ: \*4 OCPU");
        // A lone backslash stays a single literal backslash.
        assert_eq!(format_description_text(r"a\b", true), r"a\\b");
    }

    #[test]
    fn normalizes_padded_bold() {
        // M3: "**Oracle **" -> "**Oracle**" so Discord actually bolds it.
        assert_eq!(
            format_description_text("**Oracle **", true),
            "**Oracle**"
        );
        assert_eq!(
            format_description_text("** spaced **", true),
            "**spaced**"
        );
    }

    #[test]
    fn escaped_bold_stays_literal() {
        // A FB-escaped "\*\*x\*\*" is NOT bold — the escapes are honored.
        assert_eq!(
            format_description_text(r"\*\*x\*\*", true),
            r"\*\*x\*\*"
        );
    }
```

**Verify**: `cargo test --lib embed` → all pass, including the 4 new tests.

## Test plan

- New tests (all in `src/embed.rs`'s `mod tests`, modeled on the existing
  `format_description_text` tests at `src/embed.rs:436-473`):
  - `strips_leading_heading_markers` — M1 happy path + mid-line `#` left alone +
    over-long `#######` not treated as a heading.
  - `honors_fb_backslash_escape` — M2: `\*` → single literal `*`; lone `\` → `\`.
  - `normalizes_padded_bold` — M3: trailing/leading inner padding trimmed.
  - `escaped_bold_stays_literal` — regression guard for the ordering hazard the
    old escape/unescape pipeline had.
- Updated test: `preserves_intentional_discord_markdown` (heading marker now
  stripped).
- Verification: `cargo test` → all pass (84 prior, minus the one whose
  expectation changed but still counts as passing, plus 4 new).

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo build` exits 0
- [ ] `cargo fmt -- --check` exits 0 (no diff)
- [ ] `cargo test` exits 0; the 4 new tests above exist and pass
- [ ] `grep -n "unescape_paired_marker\|unescape_line_start_blockquotes" src/embed.rs`
      returns no matches (the old helpers are gone)
- [ ] `grep -n "fn escape_markdown" src/embed.rs` still returns one match
      (the `allow=false` path is preserved)
- [ ] No files outside `src/embed.rs` are modified (`git status --porcelain`
      lists only `src/embed.rs` plus this `plans/` index update)
- [ ] `plans/README.md` status row for plan 010 updated to DONE

## STOP conditions

Stop and report back (do not improvise) if:

- The drift check shows `src/embed.rs` changed since `3148b75` and the
  "Current state" excerpt no longer matches the live code.
- Any of the three unchanged tests (`escapes_markdown_when_not_allowed`,
  `escapes_accidental_markdown_punctuation`, `leaves_unmatched_bold_marker_escaped`)
  fails after the rewrite — that means the `allow=false` path or an existing
  `allow=true` behavior regressed, which is out of scope.
- A step's verification fails twice after a reasonable fix attempt.
- You find the fix needs to touch `src/parsers/json_post.rs` or any file outside
  `src/embed.rs`.

## Maintenance notes

For the human/agent who owns this code after the change lands:

- **Reviewer focus**: the `render_inline` two-pass logic. Confirm the `markers`
  pre-pass skips escaped `\*` (the `i += 2` on backslash) so a FB-escaped `\*\*`
  never becomes accidental bold, and that the close-marker padding trim
  (`while out.ends_with(' ')`) only ever removes padding adjacent to a `**`
  close, never legitimate leading whitespace from another line (each line is
  rendered into a fresh segment, so the only trailing spaces at a close marker
  are inner padding).
- **Discord's embed-description Markdown subset is the constraint**, not
  CommonMark in general. Headings and lists are *not* rendered in embed
  descriptions (only in regular messages), which is why M1 strips rather than
  renders them. If Discord later supports headings in embed descriptions, this
  strip can become a passthrough.
- **Deferred, not in scope**: italics (`*x*` / `_x_`), strikethrough (`~~`),
  spoilers (`||`), and inline code spans are all currently escaped to literal,
  not rendered. Only `**bold**` and `> blockquote` are rendered. Widening the
  rendered subset is a separate decision — the same `render_inline` pairing
  approach would extend to `~~`/`||`/`` ` `` if wanted, but each adds
  flanking/edge cases and was intentionally left out here.
- **`MD_ESCAPE` vs `is_md_special`** differ on purpose: `#` and `\` are honored
  as escape targets but are not themselves escaped when lone (a lone `#` is a
  harmless hashtag; a lone `\` becomes `\\`). Keep them in sync if the special
  set changes.
