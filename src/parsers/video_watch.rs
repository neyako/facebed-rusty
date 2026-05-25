use crate::error::{FacebedError, FacebedResult};
use crate::fetch::get_json_blocks;
use crate::jq;
use crate::parsers::util::{human_format, thumbnail_in_node, val_str_at, video_link_in_node};
use crate::parsers::{ParsedPost, Parser, ParserCtx};
use crate::url_clean::ensure_absolute;
use once_cell::sync::Lazy;
use regex::Regex;
use scraper::{Html, Selector};
use serde_json::Value;

pub struct VideoWatchParser;

static WATCH_FEED_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^https?://[^/]+/watch/?$").unwrap());

#[async_trait::async_trait]
impl Parser for VideoWatchParser {
    async fn process(&self, ctx: &ParserCtx, post_path: &str) -> FacebedResult<ParsedPost> {
        let page = ctx.fetcher.fetch(post_path, true).await?;
        let html = page.parse();
        let content_node = get_content_node(&html, &page.html, &page.url)?;
        let video_link = video_link_in_node(&content_node)
            .or_else(|| {
                for bloc in get_json_blocks(&html, false) {
                    if let Some(l) = video_link_in_node(&bloc) {
                        return Some(l);
                    }
                }
                None
            })
            .ok_or_else(|| {
                FacebedError::parse_with(
                    "Invalid watch link (vn)",
                    page.html.clone(),
                    page.url.clone(),
                )
            })?;

        let post_url = ensure_absolute(post_path);
        let op_name = get_op_name(&html).ok_or_else(|| {
            FacebedError::parse_with(
                "Invalid watch link (opn)",
                page.html.clone(),
                page.url.clone(),
            )
        })?;
        let text = content_node
            .pointer("/title/text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let likes = content_node
            .pointer("/feedback/reaction_count/count")
            .cloned()
            .unwrap_or(Value::Null);
        let cmts = content_node
            .pointer("/feedback/total_comment_count")
            .cloned()
            .unwrap_or(Value::Null);
        let date = find_creation_time(&html).ok_or_else(|| {
            FacebedError::parse_with("cannot find date", page.html.clone(), page.url.clone())
        })?;

        let thumbnail = thumbnail_in_node(&content_node).or_else(|| {
            get_json_blocks(&html, false)
                .iter()
                .find_map(|b| thumbnail_in_node(b))
        });

        Ok(ParsedPost {
            author_name: op_name,
            text,
            image_links: Vec::new(),
            url: post_url,
            date,
            likes: human_format(&likes),
            comments: human_format(&cmts),
            shares: "null".into(),
            video_links: vec![video_link],
            thumbnail,
        })
    }
}

fn get_op_name(html: &Html) -> Option<String> {
    for bloc in get_json_blocks(html, false) {
        if jq::has(&bloc, &["is_additional_profile_plus"]) {
            return val_str_at(jq::first(&bloc, "owner")?, "name").map(str::to_owned);
        }
    }
    for bloc in get_json_blocks(html, false) {
        if let Some(owner) = jq::first(&bloc, "owner") {
            if owner.is_object() {
                if let Some(name) = val_str_at(owner, "name") {
                    return Some(name.to_owned());
                }
            }
        }
    }
    None
}

fn get_content_node(html: &Html, raw_html: &str, url: &str) -> FacebedResult<Value> {
    for bloc in get_json_blocks(html, true) {
        if jq::has(
            &bloc,
            &["comment_rendering_instance", "video_view_count_renderer"],
        ) {
            if let Some(d) = jq::first(&bloc, "result").and_then(|r| r.get("data")) {
                return Ok(d.clone());
            }
        }
    }
    let canonical_sel = Selector::parse("link[rel=canonical]").unwrap();
    if let Some(el) = html.select(&canonical_sel).next() {
        if let Some(href) = el.value().attr("href") {
            if WATCH_FEED_RE.is_match(href) {
                return Err(FacebedError::no_data(
                    "Facebook served generic watch feed instead of specific video",
                ));
            }
        }
    }
    Err(FacebedError::parse_with(
        "Invalid watch link (cn)",
        raw_html.to_owned(),
        url.to_owned(),
    ))
}

fn find_creation_time(html: &Html) -> Option<i64> {
    for bloc in get_json_blocks(html, true) {
        if jq::has(&bloc, &["creation_time"]) {
            let v = jq::first(&bloc, "creation_time")?;
            return v
                .as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()));
        }
    }
    None
}
