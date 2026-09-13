use crate::parsers::{ParsedPost, ReactionKind};
use chrono::{FixedOffset, TimeZone};
use html_escape::encode_quoted_attribute;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use std::borrow::Cow;

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

pub(crate) fn escape_attr(s: &str) -> String {
    encode_quoted_attribute(s).to_string()
}

fn enc_query(s: &str) -> String {
    utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn author_label(post: &ParsedPost) -> Cow<'_, str> {
    post.author_handle
        .as_deref()
        .filter(|handle| crate::fetch::is_named_handle(handle))
        .or(post.author_id.as_deref())
        .map_or_else(
            || Cow::Borrowed(post.author_name.as_str()),
            |handle| Cow::Owned(format!("{} (@{handle})", post.author_name)),
        )
}

/// oEmbed link the embed advertises to Discord. Discord reads
/// `author_name`/`provider_name` from the linked document. Its presence does
/// not stop Discord from selecting the Activity renderer.
fn oembed_link_tag(
    engagement: &str,
    title: &str,
    url: &str,
    kind: &str,
    origin: Option<&str>,
) -> String {
    let base = origin.unwrap_or_default();
    format!(
        r#"<link rel="alternate" type="application/json+oembed" href="{base}/oembed.json?author={a}&amp;title={t}&amp;url={u}&amp;type={kind}"/>"#,
        base = base,
        a = enc_query(engagement),
        t = enc_query(title),
        u = enc_query(url),
        kind = kind,
    )
}

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

/// Markdown specials that get a literal backslash-escape so Discord renders
/// them as plain text instead of formatting.
const MD_ESCAPE: &[char] = &['*', '_', '~', '|', '`', '>'];

/// Chars a Facebook-authored `\X` escape may protect. Includes `#` and `\`
/// themselves, which `MD_ESCAPE` deliberately omits.
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

fn format_post_description(post: &ParsedPost) -> String {
    let mut output = format_description_text(&post.text, post.allow_discord_markdown);
    if let Some(context) = &post.context {
        output.push_str("\n\n> **");
        output.push_str(&escape_markdown(&context.author_name));
        output.push_str("**");
        for line in context.text.split('\n') {
            output.push_str("\n> ");
            output.push_str(&escape_markdown(line));
        }
        if !context.url.is_empty() {
            output.push_str("\n> Original post: ");
            output.push_str(&context.url);
        }
    }
    output
}

/// Render trusted FB-group-post text for a Discord embed description.
/// FB stores plain text, but group posters often write Markdown intending
/// formatting. Render a safe subset (bold, blockquote) and neutralize the rest.
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
    let ws_end = line
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    out.push_str(&line[..ws_end]);
    let mut rest = &line[ws_end..];

    let hashes = rest.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && rest[hashes..].starts_with(' ') {
        rest = rest[hashes..].trim_start_matches(' ');
    }

    if rest == ">" || rest.starts_with("> ") {
        out.push('>');
        render_inline(out, &rest[1..]);
        return;
    }

    render_inline(out, rest);
}

fn render_inline(out: &mut String, s: &str) {
    let chars: Vec<char> = s.chars().collect();

    let mut markers: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2;
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

    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            if let Some(&next) = chars.get(i + 1) {
                if is_md_special(next) {
                    push_escaped(out, next);
                    i += 2;
                    continue;
                }
            }
            push_escaped(out, '\\');
            i += 1;
            continue;
        }
        if c == '*' && opens.contains(&i) {
            out.push_str("**");
            i += 2;
            while chars.get(i) == Some(&' ') {
                i += 1;
            }
            continue;
        }
        if c == '*' && closes.contains(&i) {
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

fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn truncate_with_suffix(s: &str, suffix: &str, max: usize) -> String {
    let suffix_len = suffix.chars().count();
    if suffix_len >= max {
        return truncate_chars(suffix, max).to_owned();
    }
    let body = truncate_chars(s, max - suffix_len);
    format!("{body}{suffix}")
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

fn format_reactions(
    likes: &str,
    cmts: &str,
    shares: &str,
    top_reactions: &[ReactionKind],
) -> String {
    let mut parts = Vec::new();
    if likes != "null" {
        let prefix = if top_reactions.len() == 2 {
            top_reactions
                .iter()
                .map(|reaction| reaction.emoji())
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            "❤️".to_owned()
        };
        parts.push(format!("{prefix} {likes}"));
    }
    if cmts != "null" {
        parts.push(format!("💬 {}", cmts));
    }
    if shares != "null" {
        parts.push(format!("🔁 {}", shares));
    }
    parts.join(" • ").replace(',', ".")
}

pub(crate) fn format_engagement(post: &ParsedPost) -> String {
    format_reactions(
        &post.likes,
        &post.comments,
        &post.shares,
        &post.top_reaction_ids,
    )
}

pub fn format_full_post_embed(
    post: &ParsedPost,
    tz_offset: i32,
    activity_origin: Option<&str>,
) -> String {
    let activity_origin = activity_origin.filter(|_| crate::activity::eligible(post));
    let mut images = post.image_links.clone();
    let extra = if images.len() + post.video_links.len() > 4 {
        "contains 4+ media"
    } else {
        ""
    };
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
    let post_date = if activity_origin.is_some() {
        String::new()
    } else {
        format_timestamp(post.date, tz_offset)
    };
    let reactions = format_reactions(
        &post.likes,
        &post.comments,
        &post.shares,
        &post.top_reaction_ids,
    );
    let site_name = [CREDIT, extra, &post_date]
        .into_iter()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let url_q = quote(&post.url);
    let kind = if activity_origin.is_some() {
        "rich"
    } else if post.video_links.is_empty() {
        "link"
    } else {
        "video"
    };
    let author = author_label(post);
    let oembed = oembed_link_tag(&reactions, &author, &post.url, kind, activity_origin);
    let activity = activity_origin.map_or_else(String::new, |origin| {
        crate::activity::alternate_link(post, origin)
    });
    let mut description = format_post_description(post);
    if !post.video_links.is_empty() {
        description.push_str("\n\n🎥 also contains video");
    }

    format!(
        r##"<!DOCTYPE html>
<html lang="">
<head>
    <title>{credit}</title>
    <meta charset="UTF-8"/>
    <meta property="og:title" content="{title}"/>
    <meta property="og:description" content="{desc}"/>
        <meta property="og:site_name" content="{site_name}"/>
    <meta property="og:url" content="{url_q}"/>
    {image_meta}
    <link rel="canonical" href="{url_q}"/>
    {oembed}
    {activity}
    <meta http-equiv="refresh" content="0;url={url_q}"/>
    <meta name="twitter:card" content="summary_large_image"/>
    <meta name="theme-color" content="#0866ff"/>
</head>
</html>"##,
        credit = CREDIT,
        site_name = site_name,
        title = escape_attr(&author),
        desc = escape_attr(truncate_chars(&description, 4096)),
        url_q = url_q,
        image_meta = image_meta,
        oembed = oembed,
        activity = activity,
    )
}

pub fn format_reel_post_embed(
    post: &ParsedPost,
    tz_offset: i32,
    activity_origin: Option<&str>,
) -> String {
    let activity_origin = activity_origin.filter(|_| crate::activity::eligible(post));
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
    let image_meta = post
        .thumbnail
        .as_ref()
        .map(|u| {
            let q = escape_attr(u);
            format!(
                r#"<meta property="og:image" content="{q}"/>
<meta name="twitter:image" content="{q}"/>"#
            )
        })
        .unwrap_or_default();
    let post_date = if activity_origin.is_some() {
        String::new()
    } else {
        format_timestamp(post.date, tz_offset)
    };
    let reactions = format_reactions(
        &post.likes,
        &post.comments,
        &post.shares,
        &post.top_reaction_ids,
    );
    let media_extra = if post.image_links.len() + post.video_links.len() > 4 {
        "contains 4+ media"
    } else {
        ""
    };
    let site_name = [CREDIT, media_extra, &post_date]
        .into_iter()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let url_q = quote(&post.url);
    let author = author_label(post);
    let oembed = oembed_link_tag(&reactions, &author, &post.url, "video", activity_origin);
    let activity = activity_origin.map_or_else(String::new, |origin| {
        crate::activity::alternate_link(post, origin)
    });
    let description = format_post_description(post);

    format!(
        r##"<!DOCTYPE html>
<html lang="">
<head>
    <title>{credit}</title>
    <meta charset="UTF-8"/>
    <meta property="og:title" content="{title}"/>
    <meta property="og:description" content="{desc}"/>
    <meta property="og:site_name" content="{site_name}"/>
    <meta property="og:url" content="{url_q}"/>
    <meta property="og:video:type" content="video/mp4"/>
    <meta property="twitter:player:stream:content_type" content="video/mp4"/>

    {video_meta}
    {image_meta}

    <link rel="canonical" href="{url_q}"/>
    {oembed}
    {activity}
    <meta http-equiv="refresh" content="0;url={url_q}"/>
    <meta name="twitter:card" content="player"/>
    <meta name="theme-color" content="#0866ff"/>
</head>
</html>"##,
        credit = CREDIT,
        site_name = site_name,
        title = escape_attr(&author),
        desc = escape_attr(truncate_chars(&description, 4096)),
        url_q = url_q,
        video_meta = video_meta,
        image_meta = image_meta,
        oembed = oembed,
        activity = activity,
    )
}

/// Embed for videos that are too big for Discord's media proxy to inline
/// (~25 MB). Shows the thumbnail as `og:image`, the post text as description,
/// and a "video too big to embed" hint in the site name. Click-through goes
/// to the canonical post URL.
pub fn format_oversized_video_embed(
    post: &ParsedPost,
    tz_offset: i32,
    activity_origin: Option<&str>,
) -> String {
    let activity_origin = activity_origin.filter(|_| crate::activity::eligible(post));
    let thumb = post.thumbnail.clone().unwrap_or_default();
    let post_date = if activity_origin.is_some() {
        String::new()
    } else {
        format_timestamp(post.date, tz_offset)
    };
    let reactions = format_reactions(
        &post.likes,
        &post.comments,
        &post.shares,
        &post.top_reaction_ids,
    );
    let media_extra = if post.image_links.len() + post.video_links.len() > 4 {
        "contains 4+ media"
    } else {
        ""
    };
    let site_name = [CREDIT, media_extra, &post_date]
        .into_iter()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let url_q = quote(&post.url);
    let author = author_label(post);
    let oembed = oembed_link_tag(&reactions, &author, &post.url, "video", activity_origin);
    let description = truncate_with_suffix(
        &format_post_description(post),
        "\n\n🎥 video too big to embed — click to watch on Facebook",
        4096,
    );
    let activity = activity_origin.map_or_else(String::new, |origin| {
        crate::activity::alternate_link(post, origin)
    });
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
    <meta property="og:site_name" content="{site_name}"/>
    <meta property="og:url" content="{url_q}"/>
    {image_meta}
    <link rel="canonical" href="{url_q}"/>
    {oembed}
    {activity}
    <meta http-equiv="refresh" content="0;url={url_q}"/>
    <meta name="twitter:card" content="summary_large_image"/>
    <meta name="theme-color" content="#0866ff"/>
</head>
</html>"##,
        credit = CREDIT,
        site_name = site_name,
        title = escape_attr(&author),
        desc = escape_attr(&description),
        url_q = url_q,
        image_meta = image_meta,
        oembed = oembed,
        activity = activity,
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

#[cfg(test)]
mod tests {
    use super::{
        format_description_text, format_full_post_embed, format_oversized_video_embed,
        format_reel_post_embed,
    };
    use crate::parsers::{ParsedPost, PostContext, ReactionKind};

    fn sample_post() -> ParsedPost {
        ParsedPost {
            author_name: r#"Title "quote""#.into(),
            author_id: None,
            author_handle: None,
            author_avatar_url: None,
            context: None,
            text: "body text".into(),
            allow_discord_markdown: false,
            image_links: vec!["https://img.example/p.jpg".into()],
            url: "https://www.facebook.com/x".into(),
            date: -1,
            likes: "null".into(),
            top_reaction_ids: Vec::new(),
            comments: "null".into(),
            shares: "null".into(),
            video_links: Vec::new(),
            thumbnail: None,
        }
    }

    #[test]
    fn preserves_intentional_discord_markdown() {
        let text = "> Dmm GenG oi\n# **FIFA WORLD CUP 2026**\n\n**Germany vs Curacao**";

        assert_eq!(
            format_description_text(text, true),
            "> Dmm GenG oi\n**FIFA WORLD CUP 2026**\n\n**Germany vs Curacao**"
        );
    }

    #[test]
    fn strips_leading_heading_markers() {
        assert_eq!(
            format_description_text("# ⚠️ Cảnh báo\n### Sub", true),
            "⚠️ Cảnh báo\nSub"
        );
        assert_eq!(format_description_text("a #b c", true), "a #b c");
        assert_eq!(format_description_text("####### x", true), "####### x");
    }

    #[test]
    fn honors_fb_backslash_escape() {
        assert_eq!(
            format_description_text(r"từ: \*4 OCPU", true),
            r"từ: \*4 OCPU"
        );
        assert_eq!(format_description_text(r"a\b", true), r"a\\b");
    }

    #[test]
    fn normalizes_padded_bold() {
        assert_eq!(format_description_text("**Oracle **", true), "**Oracle**");
        assert_eq!(format_description_text("** spaced **", true), "**spaced**");
    }

    #[test]
    fn escaped_bold_stays_literal() {
        assert_eq!(format_description_text(r"\*\*x\*\*", true), r"\*\*x\*\*");
    }

    #[test]
    fn escapes_markdown_when_not_allowed() {
        let text = "> Dmm GenG oi\n# **FIFA WORLD CUP 2026**";

        assert_eq!(
            format_description_text(text, false),
            r"\> Dmm GenG oi
# \*\*FIFA WORLD CUP 2026\*\*"
        );
    }

    #[test]
    fn escapes_accidental_markdown_punctuation() {
        let text = "2 * 3 > 5? `no` \\ path";

        assert_eq!(
            format_description_text(text, true),
            r"2 \* 3 \> 5? \`no\` \\ path"
        );
    }

    #[test]
    fn leaves_unmatched_bold_marker_escaped() {
        assert_eq!(
            format_description_text("Sale ** ends", true),
            r"Sale \*\* ends"
        );
    }

    #[test]
    fn full_embed_emits_image_and_escapes_attribute() {
        let html = format_full_post_embed(&sample_post(), 0, None);

        assert!(html.contains(r#"<meta property="og:image" content="https://img.example/p.jpg"/>"#));
        assert!(html.contains("Title &quot;quote&quot;"));
        assert!(!html.contains(r#"content="Title "quote""#));
    }

    #[test]
    fn full_embed_advertises_oembed_link() {
        let html = format_full_post_embed(&sample_post(), 0, None);
        let mime = ["application/json", "oembed"].join("+");
        assert!(html.contains(&format!(r#"type="{mime}""#)));
        assert!(html.contains("/oembed.json?author="));
        assert!(html.contains("&amp;type=link"));
    }

    #[test]
    fn activity_embeds_advertise_absolute_oembed_discovery() {
        let mut post = sample_post();
        post.likes = "19".into();
        post.comments = "2".into();
        post.shares = "3".into();
        post.video_links = vec!["https://video.example/v.mp4".into()];
        post.thumbnail = Some("https://img.example/video.jpg".into());

        let full = format_full_post_embed(&post, 0, Some("https://facebed.example"));
        let reel = format_reel_post_embed(&post, 0, Some("https://facebed.example"));
        let oversized = format_oversized_video_embed(&post, 0, Some("https://facebed.example"));
        let expected_author =
            "author=%E2%9D%A4%EF%B8%8F%2019%20%E2%80%A2%20%F0%9F%92%AC%202%20%E2%80%A2%20%F0%9F%94%81%203";
        let expected_title = "title=Title%20%22quote%22";

        for html in [full, reel, oversized] {
            let href = html
                .split(r#"type="application/json+oembed" href=""#)
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap();
            assert!(href.starts_with("https://facebed.example/oembed.json?"));
            assert!(href.contains(expected_author));
            assert!(href.contains(expected_title));
        }

        let fallback = format_full_post_embed(&post, 0, None);
        let href = fallback
            .split(r#"type="application/json+oembed" href=""#)
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert!(href.starts_with("/oembed.json?"));
    }

    #[test]
    fn engagement_renders_in_oembed_author_not_footer() {
        let mut post = sample_post();
        post.likes = "19".into();
        post.comments = "2".into();
        post.shares = "3".into();

        let html = format_full_post_embed(&post, 0, None);

        assert!(html.contains("author=%E2%9D%A4%EF%B8%8F%2019%20%E2%80%A2%20%F0%9F%92%AC%202%20%E2%80%A2%20%F0%9F%94%81%203"));
        assert!(!html.contains("<meta property=\"og:site_name\" content=\"facebed on Rust\n⌚"));
    }

    #[test]
    fn footer_orders_credit_media_and_date_without_engagement() {
        let mut post = sample_post();
        post.date = 1_704_067_200;
        post.likes = "19".into();
        post.image_links = (0..5)
            .map(|n| format!("https://img.example/{n}.jpg"))
            .collect();

        let html = format_full_post_embed(&post, 7, None);
        let site_name = html.split("og:site_name\" content=\"").nth(1).unwrap();
        assert!(
            site_name.find("facebed on Rust").unwrap()
                < site_name.find("contains 4+ media").unwrap()
        );
        assert!(site_name.find("contains 4+ media").unwrap() < site_name.find("⌚").unwrap());
        assert!(!site_name.contains("❤️ 19"));
        assert!(!site_name.contains("contains 4+ images"));
    }

    #[test]
    fn two_positive_reactions_use_count_order_and_single_falls_back_to_heart() {
        let mut post = sample_post();
        post.likes = "10".into();
        post.top_reaction_ids = vec![ReactionKind::Like, ReactionKind::Love];
        let html = format_full_post_embed(&post, 0, None);
        assert!(html.contains("author=%F0%9F%91%8D%20%E2%9D%A4%EF%B8%8F%2010"));

        post.top_reaction_ids = vec![ReactionKind::Love];
        let html = format_full_post_embed(&post, 0, None);
        assert!(html.contains("author=%E2%9D%A4%EF%B8%8F%2010"));
    }

    #[test]
    fn reel_and_oversized_footers_disclose_excess_media() {
        let mut post = sample_post();
        post.likes = "19".into();
        post.image_links = (0..4)
            .map(|n| format!("https://img.example/{n}.jpg"))
            .collect();
        post.video_links = vec!["https://video.example/v.mp4".into()];

        for html in [
            format_reel_post_embed(&post, 0, None),
            format_oversized_video_embed(&post, 0, None),
        ] {
            let site_name = html.split("og:site_name\" content=\"").nth(1).unwrap();
            assert!(site_name.contains("facebed on Rust\ncontains 4+ media"));
            assert!(!site_name.contains("❤️ 19"));
        }
    }

    #[test]
    fn oversized_warning_survives_description_boundary() {
        let mut post = sample_post();
        post.text = "x".repeat(4096);
        post.video_links = vec!["https://video.example/v.mp4".into()];

        let html = format_oversized_video_embed(&post, 0, None);
        assert!(html.contains("🎥 video too big to embed — click to watch on Facebook"));
    }

    #[test]
    fn embeds_keep_author_identity_in_og_title() {
        // Given
        let mut post = sample_post();
        post.author_name = "Example Author".into();
        post.author_handle = Some("example.author".into());
        let full = format_full_post_embed(&post, 0, Some("https://facebed.example"));
        post.image_links.clear();
        post.video_links = vec!["https://video.example/post.mp4".into()];
        let reel = format_reel_post_embed(&post, 0, Some("https://facebed.example"));

        // When / Then
        for html in [full, reel] {
            assert!(html.contains(
                r#"<meta property="og:title" content="Example Author (@example.author)"/>"#
            ));
            assert!(html.contains("author="));
            assert!(html.contains("title=Example%20Author%20%28%40example%2Eauthor%29"));
            assert!(html.contains("/users/example.author/statuses/"));
        }
    }

    #[test]
    fn numeric_only_author_keeps_activity_render_with_controlled_label() {
        let mut post = sample_post();
        post.author_name = "Đặng Khôi".into();
        post.author_id = Some("1321620837694852".into());
        post.author_handle = None;
        let html = format_full_post_embed(&post, 0, Some("https://facebed.example"));
        assert!(html.contains("Đặng Khôi (@1321620837694852)"));
        assert!(html.contains(r#"type="application/activity+json""#));
        assert!(html.contains("/users/1321620837694852/statuses/"));
    }

    #[test]
    fn full_embed_advertises_activity_status() {
        let post = sample_post();
        let id = crate::activity::status_id(&post.url).unwrap();

        let html = format_full_post_embed(&post, 0, Some("https://facebed.example"));

        assert!(html.contains(&format!(
            "https://facebed.example/users/facebed/statuses/{id}"
        )));
        assert!(html.contains(r#"type="application/activity+json""#));
        assert!(html.contains("&amp;type=rich"));
    }

    #[test]
    fn activity_full_embed_omits_detailed_timestamp_but_keeps_counters() {
        // Given
        let mut post = sample_post();
        post.date = 1_704_067_200;
        post.likes = "19".into();
        post.comments = "77".into();
        post.shares = "0".into();

        // When
        let html = format_full_post_embed(&post, 7, Some("https://facebed.example"));

        // Then
        assert!(!html.contains("⌚ 2024/01/01 07:00:00 UTC+7"));
        assert!(html.contains("author=%E2%9D%A4%EF%B8%8F%2019%20%E2%80%A2%20%F0%9F%92%AC%2077%20%E2%80%A2%20%F0%9F%94%81%200"));
        assert!(!html.contains("❤️ 19 • 💬 77 • 🔁 0"));
    }

    #[test]
    fn full_embed_renders_thousand_counter_without_k_suffix() {
        let mut post = sample_post();
        post.likes = "1.294".into();
        post.comments = "236".into();
        post.shares = "0".into();

        let html = format_full_post_embed(&post, 0, None);

        assert!(html.contains("author=%E2%9D%A4%EF%B8%8F%201%2E294%20%E2%80%A2%20%F0%9F%92%AC%20236%20%E2%80%A2%20%F0%9F%94%81%200"));
        assert!(!html.contains("1.294K"));
    }

    #[test]
    fn non_activity_full_embed_keeps_detailed_timestamp() {
        // Given
        let mut post = sample_post();
        post.date = 1_704_067_200;

        // When
        let html = format_full_post_embed(&post, 7, None);

        // Then
        assert!(html.contains("⌚ 2024/01/01 07:00:00 UTC+7"));
    }

    #[test]
    fn full_embed_orders_focal_text_before_original_context() {
        // Given
        let mut post = sample_post();
        post.text = "FOCALSENTINEL".into();
        post.context = Some(PostContext {
            author_name: "ORIGINALAUTHOR".into(),
            text: "ORIGINALTEXT <unsafe>".into(),
            url: "https://www.facebook.com/original/posts/456".into(),
        });

        // When
        let html = format_full_post_embed(&post, 0, Some("https://facebed.example"));

        // Then
        let focal = html.find("FOCALSENTINEL").unwrap();
        let author = html.find("ORIGINALAUTHOR").unwrap();
        let original = html.find("ORIGINALTEXT").unwrap();
        let link = html
            .find("https://www.facebook.com/original/posts/456")
            .unwrap();
        assert!(focal < author && author < original && original < link);
        assert!(!html.contains("ORIGINALTEXT <unsafe>"));
    }

    #[test]
    fn full_embed_omits_activity_status_when_disabled() {
        let html = format_full_post_embed(&sample_post(), 0, None);

        assert!(!html.contains("/users/"));
        assert!(!html.contains("application/activity+json"));
        assert!(!html.contains(r#"type="application/activity+json""#));
    }

    #[test]
    fn reel_embed_emits_video_player_card() {
        let mut post = sample_post();
        post.image_links.clear();
        post.video_links = vec!["https://video.fbcdn.net/v.mp4".into()];

        let html = format_reel_post_embed(&post, 0, Some("https://facebed.example"));

        assert!(
            html.contains(r#"<meta property="og:video" content="https://video.fbcdn.net/v.mp4"/>"#)
        );
        assert!(html.contains(r#"<meta name="twitter:card" content="player"/>"#));
        assert!(html.contains(r#"type="application/activity+json""#));
        assert!(html.contains("https://facebed.example/users/facebed/statuses/"));
        // No thumbnail set — reel embed stays video-only.
        assert!(!html.contains("og:image"));
    }

    #[test]
    fn reel_embed_emits_thumbnail_fallback_image() {
        let mut post = sample_post();
        post.image_links.clear();
        post.video_links = vec!["https://video.fbcdn.net/v.mp4".into()];
        post.thumbnail = Some("https://img.example/video.jpg".into());

        let html = format_reel_post_embed(&post, 0, None);

        assert!(
            html.contains(r#"<meta property="og:image" content="https://img.example/video.jpg"/>"#)
        );
        assert!(html
            .contains(r#"<meta name="twitter:image" content="https://img.example/video.jpg"/>"#));
        assert!(
            html.contains(r#"<meta property="og:video" content="https://video.fbcdn.net/v.mp4"/>"#)
        );
    }

    #[test]
    fn activity_video_embeds_omit_detailed_timestamp_and_keep_counters() {
        let mut post = sample_post();
        post.date = 1_704_067_200;
        post.likes = "19".into();
        post.comments = "77".into();
        post.shares = "3".into();
        post.image_links.clear();
        post.video_links = vec!["https://video.fbcdn.net/v.mp4".into()];
        post.thumbnail = Some("https://img.example/video.jpg".into());

        let reel = format_reel_post_embed(&post, 7, Some("https://facebed.example"));
        let oversized = format_oversized_video_embed(&post, 7, Some("https://facebed.example"));

        for html in [reel, oversized] {
            assert!(!html.contains("⌚ 2024/01/01 07:00:00 UTC+7"));
            assert!(html.contains("author=%E2%9D%A4%EF%B8%8F%2019%20%E2%80%A2%20%F0%9F%92%AC%2077%20%E2%80%A2%20%F0%9F%94%81%203"));
            assert!(!html.contains("❤️ 19 • 💬 77 • 🔁 3"));
        }
    }

    #[test]
    fn oversized_video_embed_advertises_activity_status() {
        let mut post = sample_post();
        post.image_links.clear();
        post.video_links = vec!["https://video.fbcdn.net/large.mp4".into()];
        post.thumbnail = Some("https://img.example/large.jpg".into());

        let html = format_oversized_video_embed(&post, 0, Some("https://facebed.example"));

        assert!(html.contains(r#"type="application/activity+json""#));
        assert!(html.contains("https://facebed.example/users/facebed/statuses/"));
        assert!(html.contains("/oembed.json?author="));
        assert!(html.contains("video too big to embed"));
        let site_name = html.split("og:site_name\" content=\"").nth(1).unwrap();
        assert!(!site_name.contains("video too big to embed"));
    }
}
