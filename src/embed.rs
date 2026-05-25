use crate::parsers::ParsedPost;
use chrono::{FixedOffset, TimeZone};
use html_escape::encode_quoted_attribute;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

const CREDIT: &str = "facebed on Rust";

/// Match Python `quote()` — percent-encode the same special chars.
const UNSAFE: &AsciiSet = &CONTROLS
    .add(b'<')
    .add(b'>')
    .add(b'"')
    .add(b'\'')
    .add(b'#')
    .add(b'%')
    .add(b'{')
    .add(b'}')
    .add(b'[')
    .add(b']')
    .add(b'|')
    .add(b'\\')
    .add(b'^')
    .add(b'~')
    .add(b'`');

pub fn quote(s: &str) -> String {
    utf8_percent_encode(s, UNSAFE).to_string()
}

fn escape_attr(s: &str) -> String {
    encode_quoted_attribute(s).to_string()
}

/// Escape markdown control chars that Discord renders inside `og:description`.
/// Without this, raw `*_~|>` `` ` `` `\` in post text accidentally bold/italic/
/// quote/code-format segments of the embed body.
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

fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn format_timestamp(ts: i64, tz_offset: i32) -> String {
    if ts < 0 {
        return String::new();
    }
    let offset_seconds = tz_offset * 3600;
    let tz = match FixedOffset::east_opt(offset_seconds) {
        Some(t) => t,
        None => return String::new(),
    };
    let dt = match tz.timestamp_opt(ts, 0).single() {
        Some(d) => d,
        None => return String::new(),
    };
    let sign = if tz_offset >= 0 { '+' } else { '-' };
    format!(
        "⌚ {} UTC{}{}",
        dt.format("%Y/%m/%d %H:%M:%S"),
        sign,
        tz_offset.abs()
    )
}

fn format_reactions(likes: &str, cmts: &str, shares: &str) -> String {
    let mut parts = Vec::new();
    if likes != "null" {
        parts.push(format!("❤️ {}", likes));
    }
    if cmts != "null" {
        parts.push(format!("💬 {}", cmts));
    }
    if shares != "null" {
        parts.push(format!("🔁 {}", shares));
    }
    parts.join(" • ").replace(',', ".")
}

pub fn format_full_post_embed(post: &ParsedPost, tz_offset: i32) -> String {
    let mut images = post.image_links.clone();
    let mut extra = String::new();
    if images.len() > 4 {
        extra.push_str("\ncontains 4+ images");
    }
    if !post.video_links.is_empty() {
        extra.push_str("\n🎥 also contains video");
    }
    images.truncate(4);
    let image_meta = images
        .iter()
        .map(|u| {
            format!(
                r#"<meta property="og:image" content="{}"/>"#,
                escape_attr(u)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let post_date = format_timestamp(post.date, tz_offset);
    let reactions = format_reactions(&post.likes, &post.comments, &post.shares);
    let url_q = quote(&post.url);

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
        credit = CREDIT,
        title = escape_attr(&post.author_name),
        desc = escape_attr(&escape_markdown(truncate_chars(&post.text, 4096))),
        post_date = post_date,
        reactions = reactions,
        extra = extra,
        url_q = url_q,
        image_meta = image_meta,
    )
}

pub fn format_reel_post_embed(post: &ParsedPost, tz_offset: i32) -> String {
    let video_meta = post
        .video_links
        .iter()
        .map(|u| {
            let q = escape_attr(u);
            format!(
                r#"<meta property="twitter:player:stream" content="{q}"/>
<meta property="og:video" content="{q}"/>
<meta property="og:video:secure_url" content="{q}"/>"#,
                q = q
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let post_date = format_timestamp(post.date, tz_offset);
    let reactions = format_reactions(&post.likes, &post.comments, &post.shares);
    let url_q = quote(&post.url);

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
{reactions}"/>
    <meta property="og:url" content="{url_q}"/>
    <meta property="og:video:type" content="video/mp4"/>
    <meta property="twitter:player:stream:content_type" content="video/mp4"/>

    {video_meta}

    <link rel="canonical" href="{url_q}"/>
    <meta http-equiv="refresh" content="0;url={url_q}"/>
    <meta name="twitter:card" content="player"/>
    <meta name="theme-color" content="#0866ff"/>
</head>
</html>"##,
        credit = CREDIT,
        title = escape_attr(&post.author_name),
        desc = escape_attr(&escape_markdown(truncate_chars(&post.text, 4096))),
        post_date = post_date,
        reactions = reactions,
        url_q = url_q,
        video_meta = video_meta,
    )
}

/// Embed for videos that are too big for Discord's media proxy to inline
/// (~25 MB). Shows the thumbnail as `og:image`, the post text as description,
/// and a "video too big to embed" hint in the site name. Click-through goes
/// to the canonical post URL.
pub fn format_oversized_video_embed(post: &ParsedPost, tz_offset: i32) -> String {
    let thumb = post.thumbnail.clone().unwrap_or_default();
    let post_date = format_timestamp(post.date, tz_offset);
    let reactions = format_reactions(&post.likes, &post.comments, &post.shares);
    let url_q = quote(&post.url);
    let image_meta = if thumb.is_empty() {
        String::new()
    } else {
        format!(
            r#"<meta property="og:image" content="{}"/>"#,
            escape_attr(&thumb)
        )
    };
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
{reactions}
🎥 video too big to embed — click to watch on Facebook"/>
    <meta property="og:url" content="{url_q}"/>
    {image_meta}
    <link rel="canonical" href="{url_q}"/>
    <meta http-equiv="refresh" content="0;url={url_q}"/>
    <meta name="twitter:card" content="summary_large_image"/>
    <meta name="theme-color" content="#0866ff"/>
</head>
</html>"##,
        credit = CREDIT,
        title = escape_attr(&post.author_name),
        desc = escape_attr(&escape_markdown(truncate_chars(&post.text, 4096))),
        post_date = post_date,
        reactions = reactions,
        url_q = url_q,
        image_meta = image_meta,
    )
}

pub fn format_error_embed(original_url: &str, error_code: &str) -> String {
    let suffix = if error_code.is_empty() {
        String::new()
    } else {
        format!(" [{}]", error_code)
    };
    let url_q = quote(original_url);
    format!(
        r##"<!DOCTYPE html>
<html lang="">
<head>
<meta charset="UTF-8" />
    <meta name="theme-color" content="#2c3048f" />
    <meta property="og:title" content="Log in or sign up to view{suffix}"/>
    <meta property="og:description" content="See posts, photos and more on Facebook.
@neyako for cookies donation"/>
    <meta http-equiv="refresh" content="0;url={url_q}"/>
</head>
</html>"##,
        suffix = suffix,
        url_q = url_q,
    )
}

pub fn format_timeout_embed(original_url: &str) -> String {
    let url_q = quote(original_url);
    format!(
        r##"<!DOCTYPE html>
<html lang="">
<head>
<meta charset="UTF-8" />
    <meta name="theme-color" content="#f59e0b" />
    <meta property="og:title" content="Facebook took too long [T]"/>
    <meta property="og:description" content="Facebed could not finish this embed before Discord's crawler timeout. Open the link, or retry in a moment."/>
    <meta property="og:site_name" content="{credit}"/>
    <meta property="og:url" content="{url_q}"/>
    <link rel="canonical" href="{url_q}"/>
    <meta http-equiv="refresh" content="0;url={url_q}"/>
</head>
</html>"##,
        credit = CREDIT,
        url_q = url_q,
    )
}

pub fn format_redirect_page(url: &str) -> String {
    let q = quote(url);
    let esc = escape_attr(url);
    format!(
        r#"<!DOCTYPE HTML>
<html lang="en-US">
    <head>
        <meta charset="UTF-8">
        <meta http-equiv="refresh" content="0; url={q}">
        <script type="text/javascript">
            window.location.href = "{esc}"
        </script>
        <title>redirecting...</title>
    </head>
    <body>
    </body>
</html>"#,
        q = q,
        esc = esc,
    )
}

pub fn credit() -> &'static str {
    CREDIT
}
