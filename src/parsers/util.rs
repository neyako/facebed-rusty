use crate::error::FacebedError;
use crate::fetch::profile_handle_from_url;
use crate::jq;
use crate::parsers::PostContext;
use serde_json::Value;
use url::Url;

/// Python `Utils.human_format`. Integer → `1.23K`/`4.5M` style, non-int → unchanged.
pub fn human_format(num: &Value) -> String {
    let n_opt = match num {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    };
    let Some(n) = n_opt else {
        return match num {
            Value::String(s) => s.clone(),
            _ => num.to_string(),
        };
    };

    let mut f = n as f64;
    let mut magnitude = 0;
    while f.abs() >= 1000.0 {
        magnitude += 1;
        f /= 1000.0;
    }
    let suffix = ["", "K", "M", "B", "T"]
        .get(magnitude)
        .copied()
        .unwrap_or("T");
    let mut s = format!("{:.3}", (f * 1000.0).round() / 1000.0);
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    format!("{}{}", s, suffix)
}

pub fn val_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        _ => v.to_string(),
    }
}

pub fn val_str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

pub fn author_id_in_node(node: &Value) -> Option<String> {
    let node = node.get("owner_as_page").unwrap_or(node);
    match node.get("id")? {
        Value::String(id) if !id.is_empty() => Some(id.clone()),
        Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

pub fn author_handle_in_node(node: &Value) -> Option<String> {
    let node = node.get("owner_as_page").unwrap_or(node);
    val_str_at(node, "username")
        .filter(|username| !username.is_empty())
        .map(str::to_owned)
        .or_else(|| val_str_at(node, "url").and_then(profile_handle_from_url))
}

pub fn author_avatar_in_node(node: &Value) -> Option<String> {
    let node = node.get("owner_as_page").unwrap_or(node);
    for key in [
        "profile_picture",
        "profile_picture_depth_0",
        "profile_picture_depth_1",
        "profile_pic_url",
        "profile_pic_url_hd",
        "profilePictureUrl",
    ] {
        let Some(value) = node.get(key) else {
            continue;
        };
        let Some(raw) = value
            .as_str()
            .or_else(|| value.get("uri").and_then(Value::as_str))
            .or_else(|| value.get("url").and_then(Value::as_str))
        else {
            continue;
        };
        let Ok(url) = Url::parse(raw) else {
            continue;
        };
        if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() {
            return Some(raw.to_owned());
        }
    }
    None
}

/// `Story` from Python — used by JsonParser. Recursive: attached_story is the shared/quoted post.
pub struct Story<'a> {
    pub author_name: String,
    pub author_handle: Option<String>,
    pub author_avatar_url: Option<String>,
    pub text: String,
    pub image_links: Vec<String>,
    pub video_links: Vec<String>,
    pub url: String,
    pub author_id: String,
    pub attached: Option<Box<Story<'a>>>,
    _phantom: std::marker::PhantomData<&'a ()>,
}

impl<'a> Story<'a> {
    pub fn from_json(story_json: &Value) -> Result<Self, FacebedError> {
        let actors = story_json
            .get("actors")
            .and_then(|a| a.as_array())
            .ok_or_else(|| FacebedError::parse("story.actors missing"))?;
        let actor = actors
            .first()
            .ok_or_else(|| FacebedError::parse("story.actors[0] missing"))?;
        let author_name = val_str_at(actor, "name").unwrap_or("").to_owned();
        let author_handle = author_handle_in_node(actor);
        let author_avatar_url = author_avatar_in_node(actor);
        let author_id = match actor.get("id") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        let mut text = story_json
            .get("message")
            .and_then(|m| m.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_owned();
        if let Some((title, link_url)) = extract_link_card(story_json) {
            if title.is_empty() {
                text.push_str(&format!("\n🔗 {}", link_url));
            } else {
                text.push_str(&format!("\n🔗 {}: {}", title, link_url));
            }
        }
        let url = val_str_at(story_json, "wwwURL").unwrap_or("").to_owned();

        let mut image_links = images_from_post(story_json);
        let mut video_links = videos_from_post(story_json);

        let attached = match story_json.get("attached_story") {
            Some(att) if att.is_object() && att.get("actors").is_some() => {
                let inner = Story::from_json(att)?;
                for u in &inner.image_links {
                    if !image_links.contains(u) {
                        image_links.push(u.clone());
                    }
                }
                for u in &inner.video_links {
                    if !video_links.contains(u) {
                        video_links.push(u.clone());
                    }
                }
                Some(Box::new(inner))
            }
            _ => None,
        };

        Ok(Self {
            author_name,
            author_handle,
            author_avatar_url,
            text,
            image_links,
            video_links,
            url,
            author_id,
            attached,
            _phantom: std::marker::PhantomData,
        })
    }

    pub fn get_text(&self) -> String {
        self.text.clone()
    }

    pub fn context(&self) -> Option<PostContext> {
        self.attached.as_deref().map(|attached| PostContext {
            author_name: attached.author_name.clone(),
            text: attached.get_text().trim().to_owned(),
            url: attached.url.clone(),
        })
    }
}

/// Image URLs from any post-json subtree. Mirrors Story.get_image_links_post_json.
pub fn images_from_post(post_json: &Value) -> Vec<String> {
    let all_attachments = jq::all(post_json, "attachment");

    // multi-image: any attachment with `*subattachments` containing `viewer_image`
    for attachment_set in &all_attachments {
        let sub: Vec<&Value> = match attachment_set {
            Value::Object(map) => map
                .iter()
                .filter(|(k, v)| k.ends_with("subattachments") && v.get("nodes").is_some())
                .map(|(_, v)| v)
                .collect(),
            _ => Vec::new(),
        };
        if sub.is_empty() {
            continue;
        }
        let max_count = sub
            .iter()
            .filter_map(|s| s.get("nodes").and_then(|n| n.as_array()).map(|a| a.len()))
            .max()
            .unwrap_or(0);
        let candidates: Vec<&Value> = sub
            .into_iter()
            .filter(|s| {
                s.get("nodes")
                    .and_then(|n| n.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0)
                    == max_count
                    && !jq::all(s, "viewer_image").is_empty()
            })
            .collect();
        if let Some(first) = candidates.into_iter().next() {
            let imgs: Vec<String> = jq::all(first, "viewer_image")
                .into_iter()
                .filter_map(|v| val_str_at(v, "uri").map(str::to_owned))
                .collect();
            if !imgs.is_empty() {
                return imgs;
            }
        }
    }

    // single-set: attachment with "media" but not a Sticker
    for attachment_set in &all_attachments {
        if attachment_set.get("media").is_some() {
            let is_sticker = jq::all(attachment_set, "__typename")
                .into_iter()
                .any(|v| v.as_str() == Some("Sticker"));
            if is_sticker {
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

    // fallback: comet_photo_attachment_resolution_renderer.image.uri
    for aa in jq::all(post_json, "comet_photo_attachment_resolution_renderer") {
        if let Some(uri) = aa
            .get("image")
            .and_then(|i| i.get("uri"))
            .and_then(|s| s.as_str())
        {
            return vec![uri.to_owned()];
        }
    }
    Vec::new()
}

/// Video URLs from any post-json subtree.
pub fn videos_from_post(post_json: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for attachment_set in jq::all(post_json, "attachment") {
        if let Some(link) = video_link_in_node(attachment_set) {
            if !out.contains(&link) {
                out.push(link);
            }
        }
    }
    out
}

/// Bug-1 fix lives here: probe the modern progressive_url shape AND the legacy
/// browser_native_*_url shape, since FB may serve either depending on context.
pub fn video_link_in_node(node: &Value) -> Option<String> {
    // Modern: videoDeliveryResponseFragment.videoDeliveryResponseResult.progressive_urls[].progressive_url
    for fragment in jq::all(node, "videoDeliveryResponseFragment") {
        for url_obj in jq::all(fragment, "progressive_url") {
            if let Some(s) = url_obj.as_str() {
                if !s.is_empty() {
                    return Some(s.to_owned());
                }
            }
        }
    }
    // Legacy:
    for legacy in jq::all(node, "videoDeliveryLegacyFields") {
        if legacy.is_null() {
            continue;
        }
        for key in ["browser_native_hd_url", "browser_native_sd_url"] {
            if let Some(v) = jq::first(legacy, key).and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    return Some(v.to_owned());
                }
            }
        }
    }
    // Direct: playable_url (used by Stories and some reels)
    if let Some(v) = jq::first(node, "playable_url_quality_hd").and_then(|v| v.as_str()) {
        if !v.is_empty() {
            return Some(v.to_owned());
        }
    }
    if let Some(v) = jq::first(node, "playable_url").and_then(|v| v.as_str()) {
        if !v.is_empty() {
            return Some(v.to_owned());
        }
    }
    None
}

/// Best-effort thumbnail/preview image URL for a video node. Walks the common
/// shapes FB uses: `preferred_thumbnail.image.uri`, `image.uri`, and
/// `thumbnailImage.uri`. Returns the first non-empty hit.
pub fn thumbnail_in_node(node: &Value) -> Option<String> {
    let candidates = [
        &["preferred_thumbnail", "image", "uri"][..],
        &["thumbnailImage", "uri"][..],
        &["image", "uri"][..],
    ];
    for path in candidates {
        let mut cur = node;
        let mut ok = true;
        for seg in path {
            match cur.get(*seg) {
                Some(v) => cur = v,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            if let Some(s) = cur.as_str() {
                if !s.is_empty() {
                    return Some(s.to_owned());
                }
            }
        }
    }
    // Fallback: scan any nested `preferred_thumbnail` / `thumbnailImage`.
    for key in ["preferred_thumbnail", "thumbnailImage"] {
        if let Some(t) = jq::first(node, key) {
            if let Some(uri) = t
                .get("image")
                .and_then(|i| i.get("uri"))
                .and_then(|s| s.as_str())
            {
                if !uri.is_empty() {
                    return Some(uri.to_owned());
                }
            }
            if let Some(uri) = t.get("uri").and_then(|s| s.as_str()) {
                if !uri.is_empty() {
                    return Some(uri.to_owned());
                }
            }
        }
    }
    None
}

/// Extract the first link-card attachment from a story subtree: `(title, url)`
/// for `attachment.target.external_url`. FB posts that are just a shared link
/// (with optional preview title) have no `message.text`, so without this the
/// embed body comes back empty. `title` may be empty when FB renders only the
/// URL. Scan is recursive — same behavior as the upstream JS reference.
pub fn extract_link_card(story_json: &Value) -> Option<(String, String)> {
    for attachment in jq::all(story_json, "attachment") {
        let Some(target) = attachment.get("target") else {
            continue;
        };
        let Some(url) = target.get("external_url").and_then(|u| u.as_str()) else {
            continue;
        };
        if url.is_empty() {
            continue;
        }
        let title = attachment
            .get("title_with_entities")
            .and_then(|t| t.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_owned();
        return Some((title, url.to_owned()));
    }
    None
}

pub fn interaction_counts(
    post_json: &Value,
    post_id: Option<&str>,
) -> Result<(String, String, String), FacebedError> {
    let renderers = jq::all(post_json, "comet_ufi_summary_and_actions_renderer");
    let pf = post_id
        .and_then(|id| {
            renderers.iter().copied().find(|renderer| {
                renderer
                    .pointer("/feedback/subscription_target_id")
                    .is_some_and(|value| val_str(value) == id)
            })
        })
        .or_else(|| renderers.first().copied())
        .ok_or_else(|| FacebedError::parse("missing comet_ufi_summary_and_actions_renderer"))?;
    let fb = pf
        .get("feedback")
        .ok_or_else(|| FacebedError::parse("missing feedback"))?;
    let adaptive = fb
        .get("adaptive_ufi_action_renderers")
        .and_then(Value::as_array);
    let reactions = adaptive
        .and_then(|items| {
            items.iter().find_map(|item| {
                jq::first(item, "reaction_count").and_then(|count| count.get("count"))
            })
        })
        .map(human_format)
        .or_else(|| fb.get("i18n_reaction_count").map(val_str))
        .unwrap_or_else(|| "0".into());
    let shares = adaptive
        .and_then(|items| {
            items.iter().find_map(|item| {
                jq::first(item, "share_count").and_then(|count| count.get("count"))
            })
        })
        .map(human_format)
        .or_else(|| fb.get("i18n_share_count").map(val_str))
        .unwrap_or_else(|| "0".into());
    let comments = adaptive
        .and_then(|items| {
            items.iter().find_map(|item| {
                jq::first(item, "comment_rendering_instance")
                    .and_then(|comments| comments.get("comments"))
                    .and_then(|comments| comments.get("total_count"))
            })
        })
        .map(human_format)
        .or_else(|| {
            fb.get("comment_rendering_instance")
                .and_then(|comments| comments.get("comments"))
                .and_then(|comments| comments.get("total_count"))
                .map(val_str)
        })
        .unwrap_or_else(|| "0".into());
    Ok((reactions, comments, shares))
}

#[cfg(test)]
mod tests {
    use super::{
        author_avatar_in_node, author_handle_in_node, author_id_in_node, images_from_post,
        interaction_counts, Story,
    };
    use serde_json::json;

    #[test]
    fn selected_author_identity_uses_profile_fields() {
        // Given
        let author = json!({
            "id": "100012345",
            "url": "https://www.facebook.com/example.author",
            "profile_picture": {"uri": "https://scontent.example/avatar.jpg"}
        });

        // When / Then
        assert_eq!(author_id_in_node(&author).as_deref(), Some("100012345"));
        assert_eq!(
            author_handle_in_node(&author).as_deref(),
            Some("example.author")
        );
        assert_eq!(
            author_avatar_in_node(&author).as_deref(),
            Some("https://scontent.example/avatar.jpg")
        );
    }

    #[test]
    fn selected_author_avatar_supports_facebook_and_instagram_shapes() {
        // Given
        let cases = [
            (
                json!({"profile_picture_depth_0": {"uri": "https://img.example/depth.jpg"}}),
                "https://img.example/depth.jpg",
            ),
            (
                json!({"profile_pic_url": "https://img.example/profile.jpg"}),
                "https://img.example/profile.jpg",
            ),
            (
                json!({"profilePictureUrl": {"url": "https://img.example/camel.jpg"}}),
                "https://img.example/camel.jpg",
            ),
        ];

        // When / Then
        for (author, expected) in cases {
            assert_eq!(author_avatar_in_node(&author).as_deref(), Some(expected));
        }
    }

    #[test]
    fn selected_author_identity_rejects_missing_or_invalid_values() {
        // Given
        let author = json!({
            "id": "",
            "url": "https://www.facebook.com/profile.php?id=100",
            "profile_picture": {"uri": "ftp://img.example/avatar.jpg"}
        });

        // When / Then
        assert_eq!(author_id_in_node(&author), None);
        assert_eq!(author_handle_in_node(&author), None);
        assert_eq!(author_avatar_in_node(&author), None);
    }

    #[test]
    fn skips_sticker_attachment() {
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

    #[test]
    fn story_extracts_author_text_and_photo() {
        let story = Story::from_json(&json!({
            "actors": [{
                "name": "Test Author",
                "id": "100",
                "url": "https://www.facebook.com/test.author",
                "profile_picture": {"uri": "https://img.example/author.jpg"}
            }],
            "message": {"text": "hello world"},
            "wwwURL": "https://www.facebook.com/groups/1/posts/2",
            "attachment": {
                "media": {"__typename": "Photo"},
                "photo_image": {"uri": "https://img.example/p.jpg"}
            }
        }))
        .unwrap();

        assert_eq!(story.author_name, "Test Author");
        assert_eq!(story.author_id, "100");
        assert_eq!(story.author_handle.as_deref(), Some("test.author"));
        assert_eq!(
            story.author_avatar_url.as_deref(),
            Some("https://img.example/author.jpg")
        );
        assert_eq!(story.text, "hello world");
        assert_eq!(story.url, "https://www.facebook.com/groups/1/posts/2");
        assert_eq!(
            story.image_links,
            vec!["https://img.example/p.jpg".to_string()]
        );
        assert!(story.video_links.is_empty());
    }

    #[test]
    fn story_extracts_progressive_video() {
        let story = Story::from_json(&json!({
            "actors": [{"name": "V", "id": "7"}],
            "message": {"text": "vid"},
            "wwwURL": "https://www.facebook.com/x",
            "attachment": {
                "media": {
                    "videoDeliveryResponseFragment": {
                        "videoDeliveryResponseResult": {
                            "progressive_urls": [
                                {"progressive_url": "https://video.fbcdn.net/v.mp4"}
                            ]
                        }
                    }
                }
            }
        }))
        .unwrap();

        assert_eq!(
            story.video_links,
            vec!["https://video.fbcdn.net/v.mp4".to_string()]
        );
    }

    #[test]
    fn story_keeps_shared_attached_story_as_separate_context() {
        let story = Story::from_json(&json!({
            "actors": [{"name": "Outer", "id": "1"}],
            "message": {"text": "outer text"},
            "wwwURL": "https://www.facebook.com/o",
            "attached_story": {
                "actors": [{"name": "Inner", "id": "2"}],
                "message": {"text": "inner text"},
                "wwwURL": "https://www.facebook.com/i"
            }
        }))
        .unwrap();

        assert_eq!(story.get_text(), "outer text");
        let context = story.context().unwrap();
        assert_eq!(context.author_name, "Inner");
        assert_eq!(context.text, "inner text");
        assert_eq!(context.url, "https://www.facebook.com/i");
    }

    #[test]
    fn interaction_counts_selects_focal_adaptive_ufi_renderer() {
        // Given
        let post = json!({
            "payload": [
                {
                    "comet_ufi_summary_and_actions_renderer": {
                        "feedback": {
                            "subscription_target_id": "decoy",
                            "i18n_reaction_count": "999",
                            "i18n_share_count": "999",
                            "comment_rendering_instance": {
                                "comments": {"total_count": 999}
                            }
                        }
                    }
                },
                {
                    "comet_ufi_summary_and_actions_renderer": {
                        "feedback": {
                            "subscription_target_id": "2337103290413283",
                            "i18n_reaction_count": "0",
                            "i18n_share_count": "0",
                            "comment_rendering_instance": {
                                "comments": {"total_count": 0}
                            },
                            "adaptive_ufi_action_renderers": [
                                {"feedback": {"reaction_count": {"count": 19}}},
                                {"feedback": {"comment_rendering_instance": {
                                    "comments": {"total_count": 77}
                                }}},
                                {"feedback": {"share_count": {"count": 0}}}
                            ]
                        }
                    }
                }
            ]
        });

        // When
        let counts = interaction_counts(&post, Some("2337103290413283")).unwrap();

        // Then
        assert_eq!(counts, ("19".into(), "77".into(), "0".into()));
    }
}
