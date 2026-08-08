//! 24-hour Facebook stories. URL pattern: `/stories/<author_id>/<media_id>/...`.
//!
//! JSON layout (block usually contains `bucket`):
//! ```text
//! data.bucket
//!   ├─ owner { id, name, short_name }
//!   └─ unified_stories_with_notes.edges[0].node
//!        ├─ creation_time
//!        ├─ story_card_info.permalink_info.uri    (canonical url)
//!        └─ attachments[0].media
//!             ├─ playable_url           (video story)
//!             └─ image.uri              (photo story)
//! ```

use crate::error::{FacebedError, FacebedResult};
use crate::fetch::get_json_blocks;
use crate::jq;
use crate::parsers::util::{
    author_avatar_in_node, author_handle_in_node, author_id_in_node, val_str_at,
};
use crate::parsers::{banned_post, ParsedPost, Parser, ParserCtx};
use crate::url_clean::ensure_absolute;
use serde_json::Value;

pub struct StoriesParser;

#[async_trait::async_trait]
impl Parser for StoriesParser {
    async fn process(&self, ctx: &ParserCtx, post_path: &str) -> FacebedResult<ParsedPost> {
        let page = ctx.fetcher.fetch(post_path, true).await?;
        let html = page.document();
        let blocks = get_json_blocks(html, true);
        let (bucket, node) = find_story_bucket_and_node(&blocks).ok_or_else(|| {
            // No bucket = expired or login wall. Treat as NoData (24h-old stories vanish).
            FacebedError::no_data(format!(
                "story unavailable for {} (expired or restricted)",
                post_path
            ))
        })?;

        let owner = bucket.get("owner");
        let author_name = owner
            .and_then(|o| val_str_at(o, "name"))
            .unwrap_or("")
            .to_owned();
        let author_id = owner
            .and_then(|o| val_str_at(o, "id"))
            .unwrap_or("")
            .to_owned();

        let date = node
            .get("creation_time")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        let permalink = node
            .pointer("/story_card_info/permalink_info/uri")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| ensure_absolute(post_path));

        let media = node.pointer("/attachments/0/media").ok_or_else(|| {
            FacebedError::parse_with("Invalid story (media)", page.html.clone(), page.url.clone())
        })?;

        let (image_links, video_links, thumbnail) = story_media(media).ok_or_else(|| {
            FacebedError::parse_with(
                "Invalid story (no media url)",
                page.html.clone(),
                page.url.clone(),
            )
        })?;

        if ctx.is_banned(&author_id) {
            return Ok(banned_post(&permalink));
        }

        Ok(ParsedPost {
            author_name,
            author_id: owner.and_then(author_id_in_node),
            author_handle: owner.and_then(author_handle_in_node),
            author_avatar_url: owner.and_then(author_avatar_in_node),
            context: None,
            text: String::new(),
            allow_discord_markdown: false,
            image_links,
            url: permalink,
            date,
            likes: "null".into(),
            comments: "null".into(),
            shares: "null".into(),
            video_links,
            thumbnail,
        })
    }
}

fn story_media(media: &Value) -> Option<(Vec<String>, Vec<String>, Option<String>)> {
    let preview = media
        .pointer("/preferred_thumbnail/image/uri")
        .and_then(Value::as_str)
        .or_else(|| media.pointer("/image/uri").and_then(Value::as_str));
    let video = media
        .get("playable_url_quality_hd")
        .and_then(Value::as_str)
        .or_else(|| media.get("playable_url").and_then(Value::as_str));

    match (video, preview) {
        (Some(video), preview) => Some((
            Vec::new(),
            vec![video.to_owned()],
            preview.map(str::to_owned),
        )),
        (None, Some(image)) => Some((vec![image.to_owned()], Vec::new(), None)),
        (None, None) => None,
    }
}

/// Find the `(bucket, story_node)` pair for the requested story.
/// First match wins — FB usually puts the relevant bucket in the largest block.
fn find_story_bucket_and_node(blocks: &[Value]) -> Option<(Value, Value)> {
    for bloc in blocks {
        for usn in jq::all(bloc, "unified_stories_with_notes") {
            let edges = usn.get("edges").and_then(|e| e.as_array())?;
            let node = edges.first()?.get("node")?;
            if node.get("attachments").is_none() {
                continue;
            }
            // walk up: the bucket is the parent containing usn + owner
            let bucket = find_bucket_containing(bloc, usn)?;
            return Some((bucket.clone(), node.clone()));
        }
    }
    None
}

/// Locate the bucket object that owns this `unified_stories_with_notes`.
fn find_bucket_containing<'a>(root: &'a Value, needle: &Value) -> Option<&'a Value> {
    match root {
        Value::Object(map) => {
            if let Some(usn) = map.get("unified_stories_with_notes") {
                if std::ptr::eq(usn as *const _, needle as *const _) {
                    return Some(root);
                }
                // pointer equality won't work after a clone — fall back to structural match
                if usn == needle {
                    return Some(root);
                }
            }
            for v in map.values() {
                if let Some(found) = find_bucket_containing(v, needle) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(arr) => {
            for v in arr {
                if let Some(found) = find_bucket_containing(v, needle) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{find_story_bucket_and_node, story_media};
    use crate::parsers::util::{author_avatar_in_node, author_id_in_node};
    use serde_json::json;

    #[test]
    fn finds_bucket_and_story_node() {
        let blocks = vec![json!({
            "owner": {
                "id": "9",
                "name": "Story Owner",
                "profile_picture": {"uri": "https://img.example/story-owner.jpg"}
            },
            "unified_stories_with_notes": {
                "edges": [{"node": {
                    "creation_time": 123,
                    "attachments": [{"media": {"image": {"uri": "https://img.example/s.jpg"}}}]
                }}]
            }
        })];

        let (bucket, node) = find_story_bucket_and_node(&blocks).unwrap();
        assert_eq!(
            bucket.pointer("/owner/name").and_then(|v| v.as_str()),
            Some("Story Owner")
        );
        assert_eq!(
            node.get("creation_time").and_then(|v| v.as_i64()),
            Some(123)
        );
        let owner = bucket.get("owner").unwrap();
        assert_eq!(author_id_in_node(owner).as_deref(), Some("9"));
        assert_eq!(
            author_avatar_in_node(owner).as_deref(),
            Some("https://img.example/story-owner.jpg")
        );
    }

    #[test]
    fn video_story_keeps_its_preview_image_for_activity() {
        // Given
        let media = json!({
            "playable_url": "https://video.example/story.mp4",
            "preferred_thumbnail": {
                "image": {"uri": "https://img.example/story.jpg"}
            }
        });

        // When
        let (images, videos, thumbnail) = story_media(&media).unwrap();

        // Then
        assert!(images.is_empty());
        assert_eq!(videos, ["https://video.example/story.mp4"]);
        assert_eq!(thumbnail.as_deref(), Some("https://img.example/story.jpg"));
    }
}
