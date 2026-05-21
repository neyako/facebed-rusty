use crate::error::{FacebedError, FacebedResult};
use crate::fetch::get_json_blocks;
use crate::jq;
use crate::parsers::util::{human_format, val_str_at};
use crate::parsers::{ParsedPost, Parser, ParserCtx};
use scraper::Html;
use serde_json::Value;

pub struct PhotocomParser;

#[async_trait::async_trait]
impl Parser for PhotocomParser {
    async fn process(&self, ctx: &ParserCtx, post_path: &str) -> FacebedResult<ParsedPost> {
        let page = ctx.fetcher.fetch(post_path, true).await?;
        let html = page.parse();
        let content = get_content_node(&html)
            .ok_or_else(|| FacebedError::parse_with("Cannot process photocom (cn)", page.html.clone(), page.url.clone()))?;
        let data = content.get("data").ok_or_else(|| FacebedError::parse("missing data"))?;
        let attached_comment = data.get("attached_comment").ok_or_else(|| FacebedError::parse("missing attached_comment"))?;
        let body = attached_comment.get("preferred_body");
        let text = body.and_then(|b| b.get("text")).and_then(|t| t.as_str()).unwrap_or("").to_owned();
        let owner_name = data
            .pointer("/owner/name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let date = data.get("created_time").and_then(|v| v.as_i64()).unwrap_or(0);

        let (image, url) = get_attached_image_and_url(&html)
            .ok_or_else(|| FacebedError::parse_with("Cannot process photocom (iau)", page.html.clone(), page.url.clone()))?;
        let reactions_count = get_reaction_count(&html)
            .ok_or_else(|| FacebedError::parse_with("Cannot process photocom (rc)", page.html.clone(), page.url.clone()))?;

        Ok(ParsedPost {
            author_name: format!("{} (💬)", owner_name),
            text,
            image_links: vec![image],
            url,
            date,
            likes: human_format(&reactions_count.into()),
            comments: "null".into(),
            shares: "null".into(),
            video_links: Vec::new(),
            thumbnail: None,
        })
    }
}

fn get_content_node(html: &Html) -> Option<Value> {
    for bloc in get_json_blocks(html, true) {
        if jq::has(&bloc, &["attached_comment"]) && !jq::has(&bloc, &["unified_reactors"]) {
            return jq::first(&bloc, "result").cloned();
        }
    }
    None
}

fn get_reaction_count(html: &Html) -> Option<i64> {
    for bloc in get_json_blocks(html, true) {
        if jq::has(&bloc, &["attached_comment", "unified_reactors"]) {
            return jq::first(&bloc, "unified_reactors")?.get("count")?.as_i64();
        }
    }
    None
}

fn get_attached_image_and_url(html: &Html) -> Option<(String, String)> {
    for bloc in get_json_blocks(html, true) {
        if jq::has(&bloc, &["attached_comment", "unified_reactors"]) {
            let cur = jq::first(&bloc, "currMedia")?;
            let image = val_str_at(cur.get("image")?, "uri")?.to_owned();
            let url = val_str_at(cur.get("attached_comment")?.get("feedback")?, "url")?.to_owned();
            return Some((image, url));
        }
    }
    None
}
