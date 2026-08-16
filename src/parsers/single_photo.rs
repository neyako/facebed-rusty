use crate::error::{FacebedError, FacebedResult};
use crate::fetch::get_json_blocks;
use crate::jq;
use crate::parsers::util::{
    author_avatar_in_node, author_handle_in_node, author_id_in_node,
    interaction_counts_with_reactions, val_str_at,
};
use crate::parsers::{ParsedPost, Parser, ParserCtx};
use crate::url_clean::ensure_absolute;
use serde_json::Value;

pub struct SinglePhotoParser;

#[async_trait::async_trait]
impl Parser for SinglePhotoParser {
    async fn process(&self, ctx: &ParserCtx, post_path: &str) -> FacebedResult<ParsedPost> {
        let page = ctx.fetcher.fetch(post_path, true).await?;
        let html = page.document();
        let blocks = get_json_blocks(html, true);
        let content_node = get_content_node(&blocks).ok_or_else(|| {
            FacebedError::parse_with(
                "Cannot process post (cn)",
                page.html.clone(),
                page.url.clone(),
            )
        })?;
        let interaction = get_interactions_node(&blocks).ok_or_else(|| {
            FacebedError::parse_with(
                "Cannot process post (in)",
                page.html.clone(),
                page.url.clone(),
            )
        })?;
        let text = longest_post_text(&content_node);
        let owner = content_node.get("owner").unwrap_or(&Value::Null);
        let author = owner
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let date = content_node
            .get("created_time")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let (likes, cmts, shares, top_reaction_ids) =
            interaction_counts_with_reactions(&interaction, None)?;
        let image = get_single_image(&blocks).ok_or_else(|| {
            FacebedError::parse_with(
                "cannot find single image",
                page.html.clone(),
                page.url.clone(),
            )
        })?;

        Ok(ParsedPost {
            author_name: author,
            author_id: author_id_in_node(owner),
            author_handle: author_handle_in_node(owner),
            author_avatar_url: author_avatar_in_node(owner),
            context: None,
            text: text.trim().to_owned(),
            allow_discord_markdown: false,
            image_links: vec![image],
            url: ensure_absolute(post_path),
            date,
            likes,
            top_reaction_ids,
            comments: cmts,
            shares,
            video_links: Vec::new(),
            thumbnail: None,
        })
    }
}

fn get_content_node(blocks: &[Value]) -> Option<Value> {
    for bloc in blocks {
        if jq::has(bloc, &["message_preferred_body", "container_story"]) {
            return jq::first(bloc, "data").cloned();
        }
    }
    None
}

fn longest_post_text(content_node: &Value) -> &str {
    [
        content_node.pointer("/message_preferred_body/text"),
        content_node.pointer("/container_story/message/text"),
        content_node.pointer("/message/text"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .filter(|text| !text.trim().is_empty())
    .max_by_key(|text| text.chars().count())
    .unwrap_or("")
}

fn get_interactions_node(blocks: &[Value]) -> Option<Value> {
    for bloc in blocks {
        if jq::has(bloc, &["comet_ufi_summary_and_actions_renderer"]) {
            return Some(bloc.clone());
        }
    }
    None
}

fn get_single_image(blocks: &[Value]) -> Option<String> {
    for bloc in blocks {
        if jq::has(bloc, &["prefetch_uris_v2"]) {
            let first = jq::first(bloc, "prefetch_uris_v2")?.as_array()?.first()?;
            return val_str_at(first, "uri").map(str::to_owned);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{get_content_node, get_single_image, longest_post_text};
    use crate::parsers::util::{author_avatar_in_node, author_id_in_node};
    use serde_json::json;

    #[test]
    fn finds_content_node_and_single_image() {
        let blocks = vec![json!({
            "message_preferred_body": {},
            "container_story": {},
            "data": {"owner": {
                "id": "44",
                "name": "Photog",
                "profile_picture": {"uri": "https://img.example/photog.jpg"}
            }},
            "prefetch_uris_v2": [{"uri": "https://img.example/single.jpg"}]
        })];

        let content = get_content_node(&blocks).unwrap();
        let owner = content.get("owner").unwrap();
        assert_eq!(author_id_in_node(owner).as_deref(), Some("44"));
        assert_eq!(
            author_avatar_in_node(owner).as_deref(),
            Some("https://img.example/photog.jpg")
        );
        assert_eq!(
            get_single_image(&blocks).as_deref(),
            Some("https://img.example/single.jpg")
        );
    }

    #[test]
    fn prefers_full_caption_over_short_photo_preview() {
        let blocks = vec![json!({
            "message_preferred_body": {},
            "container_story": {},
            "data": {
                "message": {"text": "Short preview..."},
                "message_preferred_body": {
                    "text": "Full caption with every paragraph preserved for Discord."
                },
                "container_story": {
                    "message": {"text": "Medium caption"}
                }
            }
        })];
        let content = get_content_node(&blocks).expect("fixture must contain photo content");

        assert_eq!(
            longest_post_text(&content),
            "Full caption with every paragraph preserved for Discord."
        );
    }
}
