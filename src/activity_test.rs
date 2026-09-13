use super::{alternate_link, decode_status_path, status_id, status_json};
use crate::parsers::{ParsedPost, PostContext, ReactionKind};
use serde_json::{json, Value};
use url::Url;

fn post(text: String, image_links: Vec<&str>) -> ParsedPost {
    ParsedPost {
        author_name: "Example Author".to_owned(),
        author_id: Some("100012345".to_owned()),
        author_handle: Some("example.author".to_owned()),
        author_avatar_url: Some("https://img.example/avatar.jpg".to_owned()),
        context: None,
        text,
        allow_discord_markdown: false,
        image_links: image_links.into_iter().map(str::to_owned).collect(),
        url: "https://www.facebook.com/groups/example/posts/123".to_owned(),
        date: 1_704_067_200,
        likes: "7".to_owned(),
        top_reaction_ids: Vec::new(),
        comments: "null".to_owned(),
        shares: "2".to_owned(),
        video_links: Vec::new(),
        thumbnail: None,
    }
}

#[test]
fn status_json_uses_bare_real_handle_without_platform_domain() {
    // Given
    let post = post("qualified account".to_owned(), vec![]);

    // When
    let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();

    // Then
    assert_eq!(json["account"]["display_name"], "Example Author");
    assert_eq!(json["account"]["id"], "100012345");
    assert_eq!(json["account"]["username"], "example.author");
    assert_eq!(json["account"]["acct"], "example.author");
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
    // Given
    let post = post("matched author path".to_owned(), vec![]);
    let id = status_id(&post.url).unwrap();

    // When / Then
    assert_eq!(
        alternate_link(&post, "https://facebed.example"),
        format!(
            r#"<link href="https://facebed.example/users/example.author/statuses/{id}" rel="alternate" type="application/activity+json"/>"#
        )
    );
    let mut invalid = post;
    invalid.url = "https://example.com/x".to_owned();
    assert_eq!(alternate_link(&invalid, "https://facebed.example"), "");
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
    assert!(json["content"]
        .as_str()
        .unwrap()
        .ends_with("<strong>❤️ 7 • 🔁 2</strong>"));
    assert!(!json["content"].as_str().unwrap().contains("💬"));
    assert_eq!(json["media_attachments"].as_array().unwrap().len(), 0);
}

#[test]
fn status_json_appends_full_engagement_for_image_text_content() {
    // Given
    let mut post = post(
        "image text".to_owned(),
        vec!["https://img.example/post.jpg"],
    );
    post.likes = "83".to_owned();
    post.comments = "109".to_owned();
    post.shares = "0".to_owned();
    post.top_reaction_ids = vec![ReactionKind::Haha, ReactionKind::Like];

    // When
    let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();

    // Then
    assert!(json["content"]
        .as_str()
        .unwrap()
        .ends_with("<strong>😂 👍 83 • 💬 109 • 🔁 0</strong>"));
}

#[test]
fn status_json_appends_engagement_for_mixed_image_video_content() {
    // Given
    let mut post = post(
        "mixed media".to_owned(),
        vec!["https://img.example/post.jpg"],
    );
    post.likes = "8".to_owned();
    post.comments = "1".to_owned();
    post.shares = "4".to_owned();
    post.video_links = vec!["https://video.example/post.mp4".to_owned()];

    // When
    let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();

    // Then
    assert!(json["content"]
        .as_str()
        .unwrap()
        .ends_with("<strong>❤️ 8 • 💬 1 • 🔁 4</strong>"));
}

#[test]
fn status_json_omits_engagement_for_video_only_content() {
    // Given
    let mut post = post("video only".to_owned(), vec![]);
    post.likes = "8".to_owned();
    post.comments = "1".to_owned();
    post.shares = "4".to_owned();
    post.video_links = vec!["https://video.example/post.mp4".to_owned()];

    // When
    let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();

    // Then
    assert!(!json["content"]
        .as_str()
        .unwrap()
        .contains("❤️ 8 • 💬 1 • 🔁 4"));
}

#[test]
fn status_json_keeps_engagement_after_quoted_content() {
    // Given
    let mut post = post(
        "focal text".to_owned(),
        vec!["https://img.example/post.jpg"],
    );
    post.likes = "19".to_owned();
    post.comments = "2".to_owned();
    post.shares = "3".to_owned();
    post.context = Some(PostContext {
        author_name: "Original Author".to_owned(),
        text: "quoted text".to_owned(),
        url: "https://www.facebook.com/original/posts/456".to_owned(),
    });

    // When
    let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();
    let content = json["content"].as_str().unwrap();

    // Then
    assert_eq!(
        content,
        r#"focal text<br><br><blockquote><a href="https://www.facebook.com/original/posts/456"><strong>Quoting Original Author</strong></a><br><br>quoted text</blockquote><br><br><strong>❤️ 19 • 💬 2 • 🔁 3</strong>"#
    );
}

#[test]
fn status_json_escapes_activity_engagement() {
    // Given
    let mut post = post("text".to_owned(), vec!["https://img.example/post.jpg"]);
    post.likes = "<unsafe>".to_owned();
    post.shares = "null".to_owned();

    // When
    let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();

    // Then
    assert!(json["content"]
        .as_str()
        .unwrap()
        .ends_with("<strong>❤️ &lt;unsafe&gt;</strong>"));
}

#[test]
fn status_json_renders_safe_group_markdown_as_activity_html() {
    // Given
    let mut post = post(
        "> quoted <unsafe>&\n# **Heading**\n\\**literal**\n**unmatched".to_owned(),
        vec![],
    );
    post.allow_discord_markdown = true;

    // When
    let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();
    let content = json["content"].as_str().unwrap();

    // Then
    assert!(content.contains("<blockquote>quoted &lt;unsafe&gt;&amp;</blockquote>"));
    assert!(content.contains("<strong>Heading</strong>"));
    assert!(!content.contains("# **Heading**"));
    assert!(content.contains(r"\**literal**"));
    assert!(content.contains("**unmatched"));
}

#[test]
fn status_json_keeps_non_group_markdown_literal() {
    // Given
    let post = post("> quote\n# **Heading**".to_owned(), vec![]);

    // When
    let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();
    let content = json["content"].as_str().unwrap();

    // Then
    assert!(content.contains("&gt; quote<br># **Heading**"));
    assert!(!content.contains("<blockquote>"));
    assert!(!content.contains("<strong>Heading</strong>"));
}

#[test]
fn status_json_links_quoted_post_heading_with_fixupx_spacing() {
    // Given
    let mut post = post("focal text".to_owned(), vec![]);
    post.context = Some(PostContext {
        author_name: "Original Author".to_owned(),
        text: "original <unsafe>& text".to_owned(),
        url: "https://www.facebook.com/original/posts/456".to_owned(),
    });

    // When
    let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();
    let content = json["content"].as_str().unwrap();

    // Then
    assert_eq!(
        content,
        r#"focal text<br><br><blockquote><a href="https://www.facebook.com/original/posts/456"><strong>Quoting Original Author</strong></a><br><br>original &lt;unsafe&gt;&amp; text</blockquote><br><br><strong>❤️ 7 • 🔁 2</strong>"#
    );
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
fn status_json_uses_facebed_logo_for_avatar_and_banner_for_header() {
    // Given
    let mut post = post("text-only".to_owned(), vec![]);
    post.author_avatar_url = None;
    post.author_handle = None;
    post.author_id = None;

    // When
    let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();

    // Then
    assert_eq!(
        json["account"]["avatar"],
        "https://facebed.neyahub.com/favicon.ico"
    );
    assert_eq!(json["account"]["avatar_static"], json["account"]["avatar"]);
    assert_eq!(
        json["account"]["header"],
        "https://facebed.neyahub.com/banner.png"
    );
    assert_eq!(json["account"]["header_static"], json["account"]["header"]);
}

#[test]
fn status_json_uses_numeric_author_id_for_profile_avatar() {
    let mut post = post("numeric author".to_owned(), vec![]);
    post.author_handle = None;
    post.author_avatar_url = None;
    post.author_id = Some("61579685171950".to_owned());

    let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();

    assert_eq!(
        json["account"]["avatar"],
        "https://graph.facebook.com/61579685171950/picture?type=small"
    );
    assert_eq!(json["account"]["username"], "61579685171950");
}

#[test]
fn status_json_uses_facebook_profile_picture_when_handle_has_no_embedded_avatar() {
    // Given
    let mut post = post("text-only".to_owned(), vec![]);
    post.author_avatar_url = None;

    // When
    let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();

    // Then
    assert_eq!(
        json["account"]["avatar"],
        "https://graph.facebook.com/example.author/picture?type=small"
    );
    assert_eq!(json["account"]["avatar_static"], json["account"]["avatar"]);
}

#[test]
fn status_json_uses_real_author_avatar_when_available() {
    // Given
    let post = post("text-only".to_owned(), vec![]);

    // When
    let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();

    // Then
    assert_eq!(json["account"]["avatar"], "https://img.example/avatar.jpg");
    assert_eq!(json["account"]["avatar_static"], json["account"]["avatar"]);
}

#[test]
fn status_json_uses_video_attachment_when_post_has_no_images() {
    // Given
    let mut post = post("video".to_owned(), vec![]);
    post.video_links = vec!["https://video.example/post.mp4".to_owned()];
    post.thumbnail = Some("https://img.example/post.jpg".to_owned());

    // When
    let json: Value = serde_json::from_str(&status_json("123", &post)).unwrap();

    // Then
    assert_eq!(
        json["media_attachments"],
        json!([{
            "id": "1",
            "type": "video",
            "url": "https://video.example/post.mp4",
            "preview_url": "https://img.example/post.jpg",
            "remote_url": null,
            "preview_remote_url": null,
            "text_url": null,
            "description": null,
            "meta": {"original": {"width": 0, "height": 0}}
        }])
    );
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
