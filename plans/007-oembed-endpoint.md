# Plan 007: Serve an oEmbed JSON endpoint so Discord shows a real author/provider line

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat cbed1e3..HEAD -- src/embed.rs src/routes.rs src/main.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW-MED
- **Depends on**: none
- **Category**: direction
- **Planned at**: commit `cbed1e3`, 2026-06-17

## Why this matters

`facebed` exists to make good link previews in Discord. Today every embed is
OpenGraph-only, and the author/date/reaction metadata is crammed into a single
`og:site_name` value with literal newlines (`src/embed.rs:188-190`, `:240-242`).
That is the exact workaround every comparable tool (fxtwitter, vxtwitter,
fixupx) uses **because it has no oEmbed endpoint**. Discord reads an
`application/json+oembed` document — when present — to populate the small
attribution line above the embed title (`author_name` + clickable `author_url`)
and the provider line (`provider_name`). Adding that one endpoint plus a `<link>`
tag gives the post author its own clickable line and a clean "facebed on Rust"
provider, without removing any existing OG tag (so non-Discord crawlers such as
Telegram are unaffected).

This is additive and low-risk: a new GET route and one extra `<link>` per
embed. Nothing existing is removed.

## Current state

Files involved:

- `src/routes.rs` — axum router and all request handling. The router is small
  and explicit; you will add one route here.
- `src/embed.rs` — builds every embed's HTML `<head>`. You will add one `<link>`
  tag to the two post-embed builders.
- `src/main.rs` — module declarations (no change expected, but referenced).

### The router (`src/routes.rs:37-44`)

```rust
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/favicon.ico", get(favicon))
        .route("/banner.png", get(banner))
        .route("/*path", get(catch_all))
        .with_state(state)
}
```

### The existing HTML response helper (`src/routes.rs:78-85`) — copy its shape

```rust
fn html_response(body: String) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    (StatusCode::OK, headers, body).into_response()
}
```

`routes.rs` already imports `axum::http::{HeaderMap, HeaderValue, StatusCode}`,
`axum::response::{IntoResponse, Response}`, `axum::routing::get`, and `axum::Router`
(`src/routes.rs:17-21`).

**IMPORTANT — axum has the `json` feature OFF.** `Cargo.toml:20` builds axum with
`features = ["http1", "tokio", "matched-path", "query"]` only. So you **cannot**
use `axum::Json`. Serialize with `serde_json` (already a dependency) into a
`String` and return it with a manual `application/json` content-type, exactly
like `html_response` does for HTML. The `query` feature **is** on, so
`axum::extract::Query` is available.

### The full-post embed `<head>` (`src/embed.rs:180-198`, inside `format_full_post_embed`)

```rust
    format!(
        r##"<!DOCTYPE html>
<html lang="">
<head>
    <title>{credit}</title>
    <meta charset="UTF-8"/>
    <meta property="og:title" content="{title}"/>
    <meta property="og:description" content="{desc}"/>
    <meta property="og:site_name" content="{credit}
{post_date}
{reactions}{extra}"/>
    <meta property="og:url" content="{url_q}"/>
    {image_meta}
    <link rel="canonical" href="{url_q}"/>
    <meta http-equiv="refresh" content="0;url={url_q}"/>
    <meta name="twitter:card" content="summary_large_image"/>
    <meta name="theme-color" content="#0866ff"/>
</head>
</html>"##,
```

`format_reel_post_embed` has the same `<link rel="canonical" href="{url_q}"/>`
line at `src/embed.rs:249`.

### Existing encoding helpers in `src/embed.rs:1-32`

```rust
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
// ...
pub fn quote(s: &str) -> String {                 // for URLs in href/refresh
    utf8_percent_encode(s, UNSAFE).to_string()
}
fn escape_attr(s: &str) -> String { /* html attribute escape */ }
```

`quote()` does **not** encode `&`, `=`, `?`, or spaces, so it is wrong for
building a query string. You will add a dedicated query encoder using
`percent_encoding::NON_ALPHANUMERIC` (see Step 2).

`credit()` (`src/embed.rs:385-387`) returns the constant `"facebed on Rust"`.

### `ParsedPost` fields you will read (`src/parsers/mod.rs:14-30`)

`author_name: String`, `url: String`, `video_links: Vec<String>`. (`video_links`
empty ⇒ oembed `type` is `"link"`, otherwise `"video"`.)

## Commands you will need

| Purpose        | Command                                  | Expected on success           |
|----------------|------------------------------------------|-------------------------------|
| Build          | `cargo build`                            | exit 0, no errors             |
| Tests          | `cargo test`                             | all pass (incl. new tests)    |
| Targeted tests | `cargo test oembed`                      | new tests pass                |
| Format check   | `cargo fmt --check`                      | exit 0, no diff               |
| Format apply   | `cargo fmt`                              | rewrites files in place       |
| Manual (Discord UA) | `curl -A 'Discordbot/2.0' 'http://127.0.0.1:9812/<fb-path>'` | HTML contains the oembed `<link>` |
| Manual (endpoint)   | `curl 'http://127.0.0.1:9812/oembed.json?author=Jane&url=https://x&type=link'` | JSON with `author_name` |

CI runs `cargo test --locked` and `cargo fmt --check` (`.github/workflows/docker-build.yml:28,31`);
run those two before declaring done if you want CI parity.

## Scope

**In scope** (the only files you should modify):
- `src/routes.rs` — add `oembed` handler, `OEmbedParams`, `build_oembed_json`, `json_response`, and the route.
- `src/embed.rs` — add `enc_query` + `oembed_link_tag`, and emit it in the two post embeds.

**Out of scope** (do NOT touch):
- `src/embed.rs::format_error_embed`, `format_timeout_embed`, `format_redirect_page`,
  `format_oversized_video_embed` — error/redirect surfaces do not get oEmbed.
- The existing `og:site_name` stuffing — **leave it in place**. oEmbed is additive;
  removing it would regress non-oEmbed crawlers (Telegram).
- Any change to `og:title`, `og:image`, `og:video`, or the crawler-UA gate.
- `assets/index.html` (the stale `/text` claim there is a separate finding).

## Git workflow

- Branch: `advisor/007-oembed-endpoint`.
- Commit style (match `git log --oneline`): short imperative subject, no
  Conventional-Commits prefix. Example existing subject: `Render group post markdown`.
  Suggested: `Add oEmbed endpoint for Discord author line`.
- Do NOT push or open a PR unless the operator instructs it.

## Steps

### Step 1: Add the `/oembed.json` route, handler, and JSON builder in `src/routes.rs`

Add the route to `router()`:

```rust
        .route("/oembed.json", get(oembed))
```

Add these items near the other free functions (e.g. just below `html_response`):

```rust
fn json_response(body: String) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    (StatusCode::OK, headers, body).into_response()
}

#[derive(serde::Deserialize)]
struct OEmbedParams {
    #[serde(default)]
    author: String,
    #[serde(default)]
    url: String,
    #[serde(default, rename = "type")]
    kind: String,
}

async fn oembed(axum::extract::Query(p): axum::extract::Query<OEmbedParams>) -> Response {
    json_response(build_oembed_json(&p.author, &p.url, &p.kind))
}

/// Build the oEmbed 1.0 document Discord reads to render the author/provider
/// line. Kept pure (no extractors) so it is unit-testable.
fn build_oembed_json(author: &str, url: &str, kind: &str) -> String {
    let kind = match kind {
        "video" | "photo" | "rich" => kind,
        _ => "link",
    };
    serde_json::json!({
        "version": "1.0",
        "type": kind,
        "provider_name": crate::embed::credit(),
        "provider_url": url,
        "author_name": author,
        "author_url": url,
        "title": author,
    })
    .to_string()
}
```

**Verify**: `cargo build` → exit 0.

### Step 2: Emit the oEmbed `<link>` tag from the two post embeds in `src/embed.rs`

Add a query encoder and a tag builder near the top helpers (below `escape_attr`):

```rust
fn enc_query(s: &str) -> String {
    utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// Relative oEmbed link the embed advertises to Discord. Discord fetches this
/// href and reads `author_name`/`provider_name` from the JSON. Relative href is
/// resolved by the crawler against the page URL — see STOP conditions if a live
/// Discord test shows it is not picked up.
fn oembed_link_tag(author: &str, url: &str, kind: &str) -> String {
    format!(
        r#"<link rel="alternate" type="application/json+oembed" href="/oembed.json?author={a}&amp;url={u}&amp;type={kind}"/>"#,
        a = enc_query(author),
        u = enc_query(url),
        kind = kind,
    )
}
```

(`&amp;` is used because this is HTML attribute content. The percent-encoded
values contain no `&`, so the only literal `&` are the separators.)

In `format_full_post_embed`, just before the `format!`, compute:

```rust
    let kind = if post.video_links.is_empty() { "link" } else { "video" };
    let oembed = oembed_link_tag(&post.author_name, &post.url, kind);
```

Then insert `    {oembed}\n` into the template immediately **after** the
`<link rel="canonical" href="{url_q}"/>` line, and add `oembed = oembed,` to the
`format!` arguments.

Do the same in `format_reel_post_embed`, but with `let kind = "video";`.

**Verify**: `cargo build` → exit 0.

### Step 3: Add unit tests

In `src/routes.rs` `#[cfg(test)] mod tests`, add:

```rust
    #[test]
    fn oembed_json_has_author_and_provider() {
        let json = super::build_oembed_json("Jane Doe", "https://www.facebook.com/x", "video");
        assert!(json.contains(r#""author_name":"Jane Doe""#));
        assert!(json.contains(r#""provider_name":"facebed on Rust""#));
        assert!(json.contains(r#""type":"video""#));
    }

    #[test]
    fn oembed_json_defaults_unknown_type_to_link() {
        let json = super::build_oembed_json("A", "https://x", "garbage");
        assert!(json.contains(r#""type":"link""#));
    }
```

In `src/embed.rs` `#[cfg(test)] mod tests`, add (the existing `sample_post()`
has `author_name: r#"Title "quote""#`):

```rust
    #[test]
    fn full_embed_advertises_oembed_link() {
        let html = super::format_full_post_embed(&sample_post(), 0);
        assert!(html.contains(r#"type="application/json+oembed""#));
        assert!(html.contains("/oembed.json?author="));
    }
```

**Verify**: `cargo test oembed` → the three new tests pass. Then `cargo test` →
all pass. Then `cargo fmt --check` → exit 0 (run `cargo fmt` first if needed).

### Step 4: Manual Discord verification (maintainer step — open question)

This is the one thing the automated tests cannot confirm: whether Discord's
crawler honors a **relative** oembed href.

1. Run the server: `cargo run`.
2. `curl 'http://127.0.0.1:9812/oembed.json?author=Jane&url=https://x&type=link'`
   → confirm a JSON body with `"author_name":"Jane"`.
3. `curl -A 'Discordbot/2.0' 'http://127.0.0.1:9812/<a-real-fb-post-path>'`
   → confirm the returned HTML contains the `application/json+oembed` `<link>`.
4. Post the `facebed` URL in a Discord channel and check whether the author line
   appears above the title.

If step 4 shows **no** author line, the relative href is the likely cause. The
fallback is an absolute href built from the request `Host` header: thread the
`Host` header (already available in `catch_all` via `headers`, `src/routes.rs:154`)
down into the embed builders and prefix the href with `https://{host}`. **Do not
build that fallback pre-emptively** — it touches every `format_*` signature.
Record the Discord result in `plans/README.md` and, if the fallback is needed,
STOP and report so the host-plumbing can be planned deliberately.

## Test plan

- New unit tests: `oembed_json_has_author_and_provider`,
  `oembed_json_defaults_unknown_type_to_link` (in `routes.rs`),
  `full_embed_advertises_oembed_link` (in `embed.rs`). Model them after the
  existing `embed.rs` tests (`full_embed_emits_image_and_escapes_attribute`,
  `src/embed.rs:450`).
- Verification: `cargo test` → all pass including the 3 new tests.

## Done criteria

ALL must hold:

- [ ] `cargo build` exits 0.
- [ ] `cargo test` exits 0; the 3 new tests exist and pass.
- [ ] `cargo fmt --check` exits 0.
- [ ] `grep -n "application/json+oembed" src/embed.rs` returns 1 match.
- [ ] `grep -n "oembed" src/routes.rs` shows the route and handler.
- [ ] No files outside the in-scope list are modified (`git status`).
- [ ] The `og:site_name` blocks at `src/embed.rs:188-190` and `:240-242` are unchanged.
- [ ] `plans/README.md` status row for 007 updated (note the Step-4 Discord result).

## STOP conditions

Stop and report (do not improvise) if:

- The "Current state" excerpts do not match the live code (drift since `cbed1e3`).
- `cargo build` fails because `axum::Json` or an axum json feature is referenced —
  you must use the manual `serde_json` + `json_response` path described in Step 1.
- Step 4's Discord test shows the relative oembed href is not honored (the
  absolute-Host-header fallback is a separate, larger change — report instead).
- Implementing the field mapping seems to require changing `og:title`/`og:image`
  or removing `og:site_name` (it does not — oEmbed is purely additive).

## Maintenance notes

- The author/provider field mapping in `build_oembed_json` is the tunable
  product decision. If the maintainer prefers the reaction/date summary on the
  author line instead of the post author, that is a one-line change in
  `build_oembed_json` plus the `oembed_link_tag` arguments.
- A reviewer should confirm no existing OG tag was removed or reordered, and
  that error/redirect/timeout embeds were left without an oembed link.
- Deferred: absolute-href-via-Host-header (only if Step 4 proves relative hrefs
  are not honored), and a possible per-embed accent color via oEmbed — out of
  scope here.
