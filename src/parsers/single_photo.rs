use crate::error::{FacebedError, FacebedResult};
use crate::fetch::get_json_blocks;
use crate::jq;
use crate::parsers::util::{interaction_counts, val_str_at};
use crate::parsers::{ParsedPost, Parser, ParserCtx};
use crate::url_clean::ensure_absolute;
use scraper::Html;
use serde_json::Value;

pub struct SinglePhotoParser;

#[async_trait::async_trait]
impl Parser for SinglePhotoParser {
    async fn process(&self, ctx: &ParserCtx, post_path: &str) -> FacebedResult<ParsedPost> {
        let page = ctx.fetcher.fetch(post_path, true).await?;
        let html = page.parse();
        let content_node = get_content_node(&html).ok_or_else(|| {
            FacebedError::parse_with(
                "Cannot process post (cn)",
                page.html.clone(),
                page.url.clone(),
            )
        })?;
        let interaction = get_interactions_node(&html).ok_or_else(|| {
            FacebedError::parse_with(
                "Cannot process post (in)",
                page.html.clone(),
                page.url.clone(),
            )
        })?;
        let text = content_node
            .pointer("/message/text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let author = content_node
            .pointer("/owner/name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let date = content_node
            .get("created_time")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let (likes, cmts, shares) = interaction_counts(&interaction)?;
        let image = get_single_image(&html).ok_or_else(|| {
            FacebedError::parse_with(
                "cannot find single image",
                page.html.clone(),
                page.url.clone(),
            )
        })?;

        Ok(ParsedPost {
            author_name: author,
            text: text.trim().to_owned(),
            image_links: vec![image],
            url: ensure_absolute(post_path),
            date,
            likes,
            comments: cmts,
            shares,
            video_links: Vec::new(),
            thumbnail: None,
        })
    }
}

fn get_content_node(html: &Html) -> Option<Value> {
    for bloc in get_json_blocks(html, true) {
        if jq::has(&bloc, &["message_preferred_body", "container_story"]) {
            return jq::first(&bloc, "data").cloned();
        }
    }
    None
}

fn get_interactions_node(html: &Html) -> Option<Value> {
    for bloc in get_json_blocks(html, true) {
        if jq::has(&bloc, &["comet_ufi_summary_and_actions_renderer"]) {
            return Some(bloc);
        }
    }
    None
}

fn get_single_image(html: &Html) -> Option<String> {
    for bloc in get_json_blocks(html, true) {
        if jq::has(&bloc, &["prefetch_uris_v2"]) {
            let first = jq::first(&bloc, "prefetch_uris_v2")?.as_array()?.first()?;
            return val_str_at(first, "uri").map(str::to_owned);
        }
    }
    None
}
