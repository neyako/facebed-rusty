use crate::url_clean;
use chrono::{SecondsFormat, TimeZone, Utc};
use html_escape::{encode_quoted_attribute, encode_text};
use serde_json::json;
use url::Url;

const MAX_STATUS_BYTES: usize = 2_048;
const ACCOUNT_AVATAR_URL: &str = "https://facebed.neyahub.com/favicon.ico";
const ACCOUNT_HEADER_URL: &str = "https://facebed.neyahub.com/banner.png";

pub fn status_id(post_url: &str) -> Option<String> {
    if post_url.len() > MAX_STATUS_BYTES || !url_clean::is_facebook_page_url(post_url) {
        return None;
    }

    Some(post_url.bytes().map(|byte| format!("{byte:03}")).collect())
}

pub fn decode_status_path(id: &str) -> Option<String> {
    if id.is_empty() || id.len() % 3 != 0 || id.len() / 3 > MAX_STATUS_BYTES {
        return None;
    }

    let mut bytes = Vec::with_capacity(id.len() / 3);
    for chunk in id.as_bytes().chunks_exact(3) {
        if !chunk.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let byte = u16::from(chunk[0] - b'0') * 100
            + u16::from(chunk[1] - b'0') * 10
            + u16::from(chunk[2] - b'0');
        if byte > u16::from(u8::MAX) {
            return None;
        }
        bytes.push(u8::try_from(byte).ok()?);
    }

    let post_url = String::from_utf8(bytes).ok()?;
    if !url_clean::is_facebook_page_url(&post_url) {
        return None;
    }
    Some(url_clean::clean_path(&post_url))
}

pub fn eligible(post: &crate::parsers::ParsedPost) -> bool {
    status_id(&post.url).is_some()
}

fn activity_username(post: &crate::parsers::ParsedPost) -> &str {
    post.author_handle
        .as_deref()
        .filter(|handle| crate::fetch::is_named_handle(handle))
        .or(post.author_id.as_deref())
        .unwrap_or("facebed")
}

pub fn alternate_link(post: &crate::parsers::ParsedPost, public_origin: &str) -> String {
    if !eligible(post) {
        return String::new();
    }
    let Ok(origin) = Url::parse(public_origin) else {
        return String::new();
    };
    if !matches!(origin.scheme(), "http" | "https") || origin.host_str().is_none() {
        return String::new();
    }
    let Some(id) = status_id(&post.url) else {
        return String::new();
    };
    let Ok(mut status_url) = Url::parse(&origin.origin().ascii_serialization()) else {
        return String::new();
    };
    let Ok(mut segments) = status_url.path_segments_mut() else {
        return String::new();
    };
    // Discord selects its Activity renderer only for the canonical Mastodon
    // status shape /users/<acct>/statuses/<id>; the REST shape
    // /api/v1/statuses/<id> is fetched but never selected.
    let username = activity_username(post);
    segments
        .push("users")
        .push(username)
        .push("statuses")
        .push(&id);
    drop(segments);
    format!(r#"<link href="{status_url}" rel="alternate" type="application/activity+json"/>"#)
}

pub fn status_json(id: &str, post: &crate::parsers::ParsedPost) -> String {
    status_json_at_origin(id, post, None)
}

pub fn status_json_at_origin(
    id: &str,
    post: &crate::parsers::ParsedPost,
    origin: Option<&str>,
) -> String {
    let username = activity_username(post);
    let account_id = post.author_id.as_deref().unwrap_or(username);
    let profile_avatar = post
        .author_handle
        .as_deref()
        .filter(|handle| crate::fetch::is_named_handle(handle))
        .or(post.author_id.as_deref())
        .and_then(facebook_profile_avatar);
    let avatar = post
        .author_avatar_url
        .as_deref()
        .or(profile_avatar.as_deref())
        .unwrap_or(ACCOUNT_AVATAR_URL);
    let account_url = origin
        .map(|origin| format!("{origin}/users/{username}"))
        .unwrap_or_else(|| post.url.clone());
    let attachments = if post.image_links.is_empty() {
        post.video_links
            .first()
            .map(|url| {
                json!({
                    "id": "1", "type": "video", "url": url,
                    "preview_url": post.thumbnail, "remote_url": null,
                    "preview_remote_url": null, "text_url": null, "description": null,
                    "meta": { "original": { "width": 0, "height": 0 } },
                })
            })
            .into_iter()
            .collect::<Vec<_>>()
    } else {
        post.image_links
            .iter()
            .take(4)
            .enumerate()
            .map(|(index, url)| {
                json!({
                    "id": (index + 1).to_string(), "type": "image", "url": url,
                    "preview_url": null, "remote_url": null, "preview_remote_url": null,
                    "text_url": null, "description": null,
                    "meta": { "original": { "width": 0, "height": 0 } },
                })
            })
            .collect::<Vec<_>>()
    };
    json!({
        "id": id, "url": post.url, "uri": post.url, "created_at": created_at(post.date),
        "in_reply_to_id": null, "in_reply_to_account_id": null, "edited_at": null,
        "reblog": null, "card": null, "poll": null, "content": status_content(post),
        "spoiler_text": "", "visibility": "public", "sensitive": false, "language": null,
        "replies_count": 0, "reblogs_count": 0, "favourites_count": 0,
        "application": { "name": "Facebed", "website": null },
        "account": {
            "id": account_id, "username": username, "acct": username,
            "display_name": post.author_name, "locked": false, "bot": true,
            "discoverable": false, "group": false, "created_at": "1970-01-01T00:00:00Z",
            "note": "", "url": account_url,
            "avatar": avatar, "avatar_static": avatar,
            "header": ACCOUNT_HEADER_URL, "header_static": ACCOUNT_HEADER_URL,
            "followers_count": 0, "following_count": 0, "statuses_count": 0,
            "last_status_at": null,
        },
        "media_attachments": attachments, "mentions": [], "tags": [], "emojis": [],
    })
    .to_string()
}

fn facebook_profile_avatar(handle: &str) -> Option<String> {
    let mut url = Url::parse("https://graph.facebook.com/").ok()?;
    url.path_segments_mut().ok()?.push(handle).push("picture");
    url.query_pairs_mut().append_pair("type", "small");
    Some(url.into())
}

fn created_at(date: i64) -> String {
    match Utc.timestamp_opt(date.max(0), 0).single() {
        Some(time) => time.to_rfc3339_opts(SecondsFormat::Secs, true),
        None => "1970-01-01T00:00:00Z".to_owned(),
    }
}

fn status_content(post: &crate::parsers::ParsedPost) -> String {
    let mut content = if post.allow_discord_markdown {
        render_activity_markdown(&post.text)
    } else {
        encode_text(&post.text).to_string().replace('\n', "<br>")
    };
    if let Some(context) = &post.context {
        if !content.is_empty() {
            content.push_str("<br><br>");
        }
        content.push_str("<blockquote>");
        if !context.url.is_empty() {
            content.push_str("<a href=\"");
            content.push_str(&encode_quoted_attribute(&context.url));
            content.push_str("\">");
        }
        content.push_str("<strong>Quoting ");
        content.push_str(&encode_text(&context.author_name));
        content.push_str("</strong>");
        if !context.url.is_empty() {
            content.push_str("</a>");
        }
        if !context.text.is_empty() {
            content.push_str("<br><br>");
            content.push_str(&encode_text(&context.text).replace('\n', "<br>"));
        }
        content.push_str("</blockquote>");
    }
    if post.video_links.is_empty() || !post.image_links.is_empty() {
        let engagement = crate::embed::format_engagement(post);
        if !engagement.is_empty() {
            if !content.is_empty() {
                content.push_str("<br><br>");
            }
            content.push_str("<strong>");
            content.push_str(&encode_text(&engagement));
            content.push_str("</strong>");
        }
    }
    content
}

fn render_activity_markdown(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            output.push_str("<br>");
        }
        render_activity_markdown_line(&mut output, line);
    }
    output
}

fn render_activity_markdown_line(output: &mut String, line: &str) {
    let whitespace_end = line
        .char_indices()
        .find(|(_, character)| !character.is_whitespace())
        .map(|(index, _)| index)
        .unwrap_or(line.len());
    output.push_str(&encode_text(&line[..whitespace_end]));
    let mut rest = &line[whitespace_end..];

    let heading_markers = rest
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if (1..=6).contains(&heading_markers) && rest[heading_markers..].starts_with(' ') {
        rest = rest[heading_markers..].trim_start_matches(' ');
    }

    if rest == ">" || rest.starts_with("> ") {
        output.push_str("<blockquote>");
        render_activity_inline(output, rest.strip_prefix("> ").unwrap_or(""));
        output.push_str("</blockquote>");
        return;
    }

    // Facebook renders `* item` lines in group posts as bullet points.
    // A leading `* ` is a bullet, not a bold marker (`**` is handled inline).
    if rest == "*" || rest.starts_with("* ") {
        output.push_str("• ");
        render_activity_inline(output, rest.strip_prefix("* ").unwrap_or(""));
        return;
    }

    render_activity_inline(output, rest);
}

fn render_activity_inline(output: &mut String, text: &str) {
    // Facebook text-delighter markers: `**bold**` and `` `code` ``. Markers of
    // each kind pair up first-with-second, third-with-fourth; unmatched
    // trailing markers and escaped ones stay literal.
    let characters = text.chars().collect::<Vec<_>>();
    let mut bold_markers = Vec::new();
    let mut code_markers = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        if characters[index] == '\\' {
            index += 2;
            continue;
        }
        if characters[index] == '*'
            && characters
                .get(index + 1)
                .is_some_and(|character| *character == '*')
        {
            bold_markers.push(index);
            index += 2;
            continue;
        }
        if characters[index] == '`' {
            code_markers.push(index);
            index += 1;
            continue;
        }
        index += 1;
    }

    let (bold_opens, bold_closes) = pair_activity_markers(&bold_markers);
    let (code_opens, code_closes) = pair_activity_markers(&code_markers);

    let mut plain = String::new();
    let mut index = 0;
    while index < characters.len() {
        if characters[index] == '\\' {
            plain.push('\\');
            if let Some(next) = characters.get(index + 1) {
                plain.push(*next);
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if bold_opens.binary_search(&index).is_ok() {
            flush_activity_text(output, &mut plain);
            output.push_str("<strong>");
            index += 2;
            continue;
        }
        if bold_closes.binary_search(&index).is_ok() {
            flush_activity_text(output, &mut plain);
            output.push_str("</strong>");
            index += 2;
            continue;
        }
        if code_opens.binary_search(&index).is_ok() {
            flush_activity_text(output, &mut plain);
            output.push_str("<code>");
            index += 1;
            continue;
        }
        if code_closes.binary_search(&index).is_ok() {
            flush_activity_text(output, &mut plain);
            output.push_str("</code>");
            index += 1;
            continue;
        }
        plain.push(characters[index]);
        index += 1;
    }
    flush_activity_text(output, &mut plain);
}

fn pair_activity_markers(markers: &[usize]) -> (Vec<usize>, Vec<usize>) {
    let paired = markers.len() - (markers.len() % 2);
    let opens = markers
        .iter()
        .take(paired)
        .step_by(2)
        .copied()
        .collect::<Vec<_>>();
    let closes = markers
        .iter()
        .take(paired)
        .skip(1)
        .step_by(2)
        .copied()
        .collect::<Vec<_>>();
    (opens, closes)
}

fn flush_activity_text(output: &mut String, plain: &mut String) {
    output.push_str(&encode_text(plain));
    plain.clear();
}

#[cfg(test)]
#[path = "activity_test.rs"]
mod tests;
