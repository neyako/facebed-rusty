use crate::url_clean;
use chrono::{SecondsFormat, TimeZone, Utc};
use html_escape::encode_text;
use serde_json::json;
use url::Url;

const MAX_STATUS_BYTES: usize = 2_048;
const ACCOUNT_IMAGE_URL: &str = "https://facebed.neyahub.com/banner.png";

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

pub fn alternate_link(post_url: &str, public_origin: &str) -> String {
    let Ok(origin) = Url::parse(public_origin) else {
        return String::new();
    };
    if !matches!(origin.scheme(), "http" | "https") || origin.host_str().is_none() {
        return String::new();
    }
    let origin = origin.origin().ascii_serialization();
    status_id(post_url).map_or_else(String::new, |id| {
        format!(
            r#"<link href="{origin}/users/facebed/statuses/{id}" rel="alternate" type="application/activity+json"/>"#
        )
    })
}

pub fn status_json(id: &str, post: &crate::parsers::ParsedPost) -> String {
    let attachments = post
        .image_links
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
        .collect::<Vec<_>>();
    json!({
        "id": id, "url": post.url, "uri": post.url, "created_at": created_at(post.date),
        "in_reply_to_id": null, "in_reply_to_account_id": null, "edited_at": null,
        "reblog": null, "card": null, "poll": null, "content": status_content(post),
        "spoiler_text": "", "visibility": "public", "sensitive": false, "language": null,
        "replies_count": 0, "reblogs_count": 0, "favourites_count": 0,
        "application": { "name": "Facebed", "website": null },
        "account": {
            "id": "facebed", "username": "facebed", "acct": "facebed",
            "display_name": post.author_name, "locked": false, "bot": true,
            "discoverable": false, "group": false, "created_at": "1970-01-01T00:00:00Z",
            "note": "", "url": post.url,
            "avatar": ACCOUNT_IMAGE_URL, "avatar_static": ACCOUNT_IMAGE_URL,
            "header": ACCOUNT_IMAGE_URL, "header_static": ACCOUNT_IMAGE_URL,
            "followers_count": 0, "following_count": 0, "statuses_count": 0,
            "last_status_at": null,
        },
        "media_attachments": attachments, "mentions": [], "tags": [], "emojis": [],
    })
    .to_string()
}

fn created_at(date: i64) -> String {
    match Utc.timestamp_opt(date.max(0), 0).single() {
        Some(time) => time.to_rfc3339_opts(SecondsFormat::Secs, true),
        None => "1970-01-01T00:00:00Z".to_owned(),
    }
}

fn status_content(post: &crate::parsers::ParsedPost) -> String {
    let mut content = encode_text(&post.text).to_string().replace('\n', "<br>");
    for (icon, count) in [
        ("❤️", &post.likes),
        ("💬", &post.comments),
        ("🔁", &post.shares),
    ] {
        if count != "null" {
            content.push_str("<br>");
            content.push_str(icon);
            content.push(' ');
            content.push_str(&encode_text(count));
        }
    }
    content
}

#[cfg(test)]
#[path = "activity_test.rs"]
mod tests;
