use crate::url_clean;
use chrono::{SecondsFormat, TimeZone, Utc};
use html_escape::encode_text;
use serde_json::json;
use url::Url;

const MAX_STATUS_BYTES: usize = 2_048;

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
            "note": "", "url": post.url, "avatar": "", "avatar_static": "",
            "header": "", "header_static": "", "followers_count": 0,
            "following_count": 0, "statuses_count": 0, "last_status_at": null,
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
mod tests {
    use super::{alternate_link, decode_status_path, status_id, status_json};
    use crate::parsers::ParsedPost;
    use serde_json::{json, Value};

    fn post(text: String, image_links: Vec<&str>) -> ParsedPost {
        ParsedPost {
            author_name: "Example Author".to_owned(),
            text,
            allow_discord_markdown: false,
            image_links: image_links.into_iter().map(str::to_owned).collect(),
            url: "https://www.facebook.com/groups/example/posts/123".to_owned(),
            date: 1_704_067_200,
            likes: "7".to_owned(),
            comments: "null".to_owned(),
            shares: "2".to_owned(),
            video_links: Vec::new(),
            thumbnail: None,
        }
    }

    #[test]
    fn status_id_round_trips_facebook_path() {
        let post_url = "https://www.facebook.com/groups/example/posts/123?comment_id=9";

        let id = status_id(post_url).unwrap();

        assert!(id.bytes().all(|byte| byte.is_ascii_digit()));
        assert_eq!(
            decode_status_path(&id),
            Some("groups/example/posts/123?comment_id=9".to_owned())
        );
    }

    #[test]
    fn status_id_rejects_invalid_inputs() {
        let non_facebook_id = "https://example.com/x"
            .bytes()
            .map(|byte| format!("{byte:03}"))
            .collect::<String>();

        assert_eq!(status_id("https://example.com/x"), None);
        assert_eq!(decode_status_path("12x"), None);
        assert_eq!(decode_status_path("999"), None);
        assert_eq!(decode_status_path("12"), None);
        assert_eq!(decode_status_path(&non_facebook_id), None);
        assert_eq!(decode_status_path(&"065".repeat(2_049)), None);
        assert_eq!(decode_status_path("255"), None);
        assert_eq!(
            status_id(&format!("https://www.facebook.com/{}", "a".repeat(2_049))),
            None
        );
    }

    #[test]
    fn alternate_link_uses_absolute_origin_for_facebook_posts() {
        let post_url = "https://www.facebook.com/posts/123";
        let id = status_id(post_url).unwrap();

        assert_eq!(
            alternate_link(post_url, "https://facebed.example"),
            format!(
                r#"<link href="https://facebed.example/users/facebed/statuses/{id}" rel="alternate" type="application/activity+json"/>"#
            )
        );
        assert_eq!(
            alternate_link("https://example.com/x", "https://facebed.example"),
            ""
        );
    }

    #[test]
    fn status_json_preserves_long_escaped_text_without_media() {
        let post = post(
            format!("{}<unsafe>&\nFINAL_SENTINEL", "🙂".repeat(1_025)),
            vec![],
        );

        let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();

        assert!(json["content"].as_str().unwrap().contains("FINAL_SENTINEL"));
        assert!(json["content"]
            .as_str()
            .unwrap()
            .contains("&lt;unsafe&gt;&amp;"));
        assert!(json["content"].as_str().unwrap().contains("<br>"));
        assert!(json["content"].as_str().unwrap().contains("❤️ 7"));
        assert_eq!(json["media_attachments"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn status_json_keeps_mastodon_image_attachments_in_source_order() {
        // Given
        let post = post(
            "gallery".to_owned(),
            vec![
                "https://img.example/first.jpg",
                "https://img.example/second.jpg",
                "https://img.example/third.jpg",
            ],
        );

        // When
        let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();
        let attachments = json["media_attachments"].as_array().unwrap();

        // Then
        assert_eq!(attachments.len(), 3);
        for (index, (attachment, url)) in attachments
            .iter()
            .zip([
                "https://img.example/first.jpg",
                "https://img.example/second.jpg",
                "https://img.example/third.jpg",
            ])
            .enumerate()
        {
            assert_eq!(
                attachment,
                &json!({
                    "id": (index + 1).to_string(), "type": "image", "url": url,
                    "preview_url": null, "remote_url": null, "preview_remote_url": null,
                    "text_url": null, "description": null,
                    "meta": { "original": { "width": 0, "height": 0 } },
                })
            );
        }
    }

    #[test]
    fn status_json_limits_gallery_to_first_four_images() {
        let fifth = "https://img.example/fifth.jpg";
        let post = post(
            "gallery".to_owned(),
            vec![
                "https://img.example/first.jpg",
                "https://img.example/second.jpg",
                "https://img.example/third.jpg",
                "https://img.example/fourth.jpg",
                fifth,
            ],
        );

        let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();
        let attachments = json["media_attachments"].as_array().unwrap();

        assert_eq!(attachments.len(), 4);
        assert_eq!(
            attachments
                .iter()
                .map(|attachment| attachment["url"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "https://img.example/first.jpg",
                "https://img.example/second.jpg",
                "https://img.example/third.jpg",
                "https://img.example/fourth.jpg",
            ]
        );
        assert!(attachments
            .iter()
            .all(|attachment| attachment["url"] != fifth));
    }
}
