use crate::error::{FacebedError, FacebedResult};
use crate::fetch::get_json_blocks;
use crate::jq;
use crate::parsers::util::{
    author_avatar_in_node, author_handle_in_node, author_id_in_node, human_format,
    top_reactions_from_feedback, val_str_at,
};
use crate::parsers::{ParsedPost, Parser, ParserCtx};
use serde_json::Value;

pub struct PhotocomParser;

#[async_trait::async_trait]
impl Parser for PhotocomParser {
    async fn process(&self, ctx: &ParserCtx, post_path: &str) -> FacebedResult<ParsedPost> {
        let page = ctx.fetcher.fetch(post_path, true).await?;
        let html = page.document();
        let blocks = get_json_blocks(html, true);
        let content = get_content_node(&blocks).ok_or_else(|| {
            FacebedError::parse_with(
                "Cannot process photocom (cn)",
                page.html.clone(),
                page.url.clone(),
            )
        })?;
        let data = content
            .get("data")
            .ok_or_else(|| FacebedError::parse("missing data"))?;
        let attached_comment = data
            .get("attached_comment")
            .ok_or_else(|| FacebedError::parse("missing attached_comment"))?;
        let body = attached_comment.get("preferred_body");
        let text = body
            .and_then(|b| b.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_owned();
        let owner = data.get("owner").unwrap_or(&Value::Null);
        let owner_name = owner
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let date = data
            .get("created_time")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        let (image, url) = get_attached_image_and_url(&blocks).ok_or_else(|| {
            FacebedError::parse_with(
                "Cannot process photocom (iau)",
                page.html.clone(),
                page.url.clone(),
            )
        })?;
        let reactions_count = get_reaction_count(&blocks).ok_or_else(|| {
            FacebedError::parse_with(
                "Cannot process photocom (rc)",
                page.html.clone(),
                page.url.clone(),
            )
        })?;
        let top_reaction_ids = get_reaction_feedback(&blocks)
            .map(top_reactions_from_feedback)
            .unwrap_or_default();

        Ok(ParsedPost {
            author_name: format!("{} (💬)", owner_name),
            author_id: author_id_in_node(owner),
            author_handle: author_handle_in_node(owner),
            author_avatar_url: author_avatar_in_node(owner),
            context: None,
            text,
            allow_discord_markdown: false,
            image_links: vec![image],
            url,
            date,
            likes: human_format(&reactions_count.into()),
            top_reaction_ids,
            comments: "null".into(),
            shares: "null".into(),
            video_links: Vec::new(),
            thumbnail: None,
        })
    }
}

fn get_content_node(blocks: &[Value]) -> Option<Value> {
    for bloc in blocks {
        if jq::has(bloc, &["attached_comment"]) && !jq::has(bloc, &["unified_reactors"]) {
            return jq::first(bloc, "result").cloned();
        }
    }
    None
}

fn get_reaction_count(blocks: &[Value]) -> Option<i64> {
    for bloc in blocks {
        if jq::has(bloc, &["attached_comment", "unified_reactors"]) {
            return jq::first(bloc, "unified_reactors")?.get("count")?.as_i64();
        }
    }
    None
}

fn get_reaction_feedback(blocks: &[Value]) -> Option<&Value> {
    blocks.iter().find_map(|bloc| {
        if !jq::has(bloc, &["attached_comment", "unified_reactors"]) {
            return None;
        }
        jq::first(bloc, "currMedia")
            .and_then(|media| media.get("attached_comment"))
            .and_then(|comment| comment.get("feedback"))
    })
}

fn get_attached_image_and_url(blocks: &[Value]) -> Option<(String, String)> {
    for bloc in blocks {
        if jq::has(bloc, &["attached_comment", "unified_reactors"]) {
            let cur = jq::first(bloc, "currMedia")?;
            let image = val_str_at(cur.get("image")?, "uri")?.to_owned();
            let url = val_str_at(cur.get("attached_comment")?.get("feedback")?, "url")?.to_owned();
            return Some((image, url));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{get_attached_image_and_url, get_content_node, get_reaction_count};
    use crate::parsers::util::{author_avatar_in_node, author_id_in_node};
    use serde_json::json;

    #[test]
    fn finds_reaction_count_and_attached_image() {
        let blocks = vec![
            json!({
                "attached_comment": {},
                "result": {"data": {"owner": {
                    "id": "55",
                    "name": "Comment Owner",
                    "profile_picture_depth_0": {"uri": "https://img.example/comment-owner.jpg"}
                }}}
            }),
            json!({
                "attached_comment": {},
                "unified_reactors": {"count": 5},
                "currMedia": {
                    "image": {"uri": "https://img.example/comment.jpg"},
                    "attached_comment": {"feedback": {"url": "https://www.facebook.com/c"}}
                }
            }),
        ];

        assert_eq!(get_reaction_count(&blocks), Some(5));
        let content = get_content_node(&blocks).unwrap();
        let owner = content.pointer("/data/owner").unwrap();
        assert_eq!(author_id_in_node(owner).as_deref(), Some("55"));
        assert_eq!(
            author_avatar_in_node(owner).as_deref(),
            Some("https://img.example/comment-owner.jpg")
        );
        assert_eq!(
            get_attached_image_and_url(&blocks),
            Some((
                "https://img.example/comment.jpg".to_string(),
                "https://www.facebook.com/c".to_string()
            ))
        );
    }
}
