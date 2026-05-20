use crate::error::{FacebedError, FacebedResult};
use crate::fetch::get_json_blocks;
use crate::jq;
use crate::parsers::util::{interaction_counts, val_str_at, Story};
use crate::parsers::{banned_post, ParsedPost, Parser, ParserCtx};
use crate::url_clean::ensure_absolute;
use scraper::Html;
use serde_json::Value;

pub struct JsonPostParser;

#[async_trait::async_trait]
impl Parser for JsonPostParser {
    async fn process(&self, ctx: &ParserCtx, post_path: &str) -> FacebedResult<ParsedPost> {
        let page = ctx.fetcher.fetch(post_path, true).await?;
        let html = page.parse();
        let post_json = get_post_json(&html).ok_or_else(|| {
            FacebedError::parse_with(
                "cannot find post json",
                page.html.clone(),
                page.url.clone(),
            )
        })?;
        let root = get_root_node(&post_json)
            .ok_or_else(|| FacebedError::parse_with("Cannot process post", page.html.clone(), page.url.clone()))?;
        let (likes, cmts, shares) = interaction_counts(root)?;

        let post_date = root
            .pointer("/context_layout/story/comet_sections/metadata")
            .and_then(|m| jq::first(m, "creation_time"))
            .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(0);

        let story_json = root
            .pointer("/content/story")
            .ok_or_else(|| FacebedError::parse_with("missing content.story", page.html.clone(), page.url.clone()))?;
        let story = Story::from_json(story_json)?;

        let post_url = if story.url.is_empty() { ensure_absolute(post_path) } else { story.url.clone() };
        let post_content = story.get_text().trim().to_owned();
        let group_name = get_group_name(&html);
        let mut link_header = story.author_name.clone();
        if !group_name.is_empty() {
            link_header.push_str(" • ");
            link_header.push_str(&group_name);
        }

        if ctx.is_banned(&story.author_id) {
            return Ok(banned_post(&post_url));
        }

        Ok(ParsedPost {
            author_name: link_header,
            text: post_content,
            image_links: story.image_links,
            url: post_url,
            date: post_date,
            likes,
            comments: cmts,
            shares,
            video_links: story.video_links,
        })
    }
}

fn get_post_json(html: &Html) -> Option<Value> {
    for bloc in get_json_blocks(html, true) {
        if jq::has(&bloc, &["i18n_reaction_count"]) {
            return Some(bloc);
        }
    }
    None
}

fn get_root_node(post_json: &Value) -> Option<&Value> {
    // normal: data has comet_ufi_summary..., node_v2 or node
    let data = jq::first(post_json, "data")?;
    if data.get("comet_ufi_summary_and_actions_renderer").is_some() {
        return Some(data);
    }
    if let Some(nv2) = data.get("node_v2") {
        if let Some(cs) = nv2.get("comet_sections") {
            return Some(cs);
        }
    }
    if let Some(n) = data.get("node") {
        if let Some(cs) = n.get("comet_sections") {
            return Some(cs);
        }
    }

    // group post
    let hoisted = jq::first(post_json, "group_hoisted_feed")?;
    let cs = jq::first(hoisted, "comet_sections")?;
    Some(cs)
}

fn get_group_name(html: &Html) -> String {
    for bloc in get_json_blocks(html, true) {
        if jq::has(&bloc, &["group_member_profiles", "formatted_count_text"]) {
            for group in jq::all(&bloc, "group") {
                if let Some(name) = val_str_at(group, "name") {
                    return name.to_owned();
                }
            }
        }
    }
    String::new()
}
