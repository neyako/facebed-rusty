use super::{alternate_link, decode_status_path, status_id, status_json};
use crate::parsers::ParsedPost;
use serde_json::{json, Value};
use url::Url;

fn post(text: String, image_links: Vec<&str>) -> ParsedPost {
    ParsedPost {
        author_name: "Example Author".to_owned(),
        author_handle: Some("example.author".to_owned()),
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
fn status_json_uses_real_handle_with_short_facebook_domain() {
    // Given
    let post = post("qualified account".to_owned(), vec![]);

    // When
    let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();

    // Then
    assert_eq!(json["account"]["display_name"], "Example Author");
    assert_eq!(json["account"]["username"], "example.author");
    assert_eq!(json["account"]["acct"], "example.author@fb.com");
    assert_eq!(
        json["account"]["url"],
        "https://www.facebook.com/groups/example/posts/123"
    );
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
    assert!(!json["content"].as_str().unwrap().contains("❤️ 7"));
    assert!(!json["content"].as_str().unwrap().contains("💬"));
    assert!(!json["content"].as_str().unwrap().contains("🔁 2"));
    assert_eq!(json["media_attachments"].as_array().unwrap().len(), 0);
}

#[test]
fn status_json_uses_absolute_https_account_images() {
    // Given
    let post = post("text-only".to_owned(), vec![]);

    // When
    let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();

    // Then
    for field in ["avatar", "avatar_static", "header", "header_static"] {
        let value = json["account"][field].as_str().unwrap();
        let image = Url::parse(value).expect("account image must be an absolute URL");

        assert_eq!(
            image.scheme(),
            "https",
            "invalid account image field: {field}"
        );
        assert!(
            image.host_str().is_some(),
            "account image has no host: {field}"
        );
    }
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
