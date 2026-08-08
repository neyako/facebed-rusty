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
use url::Url;

pub struct VideoWatchParser;

static WATCH_FEED_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^https?://[^/]+/watch/?$").unwrap());

#[async_trait::async_trait]
impl Parser for VideoWatchParser {
    async fn process(&self, ctx: &ParserCtx, post_path: &str) -> FacebedResult<ParsedPost> {
        let page = ctx.fetcher.fetch(post_path, true).await?;
        let html = page.document();
        let blocks = get_json_blocks(html, true);
        let target_video_id = target_video_id(post_path);
        let content_node = get_content_node(
            &blocks,
            html,
            &page.html,
            &page.url,
            target_video_id.as_deref(),
        )?;
        let video_link = get_video_link(&blocks, &content_node, target_video_id.as_deref())
            .ok_or_else(|| {
                FacebedError::parse_with(
                    "Invalid watch link (vn)",
                    page.html.clone(),
                    page.url.clone(),
                )
            })?;

        let post_url = ensure_absolute(post_path);
        let video_id = val_str_at(&content_node, "id")
            .map(str::to_owned)
            .or(target_video_id)
            .unwrap_or_default();
        let op_name = get_op_name(&blocks, &content_node, &video_id).ok_or_else(|| {
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
        let date = find_creation_time(&blocks).ok_or_else(|| {
            FacebedError::parse_with("cannot find date", page.html.clone(), page.url.clone())
        })?;

        let thumbnail = thumbnail_in_node(&content_node)
            .or_else(|| thumbnail_in_target_blocks(&blocks, &video_id))
            .or_else(|| blocks.iter().find_map(thumbnail_in_node));

        Ok(ParsedPost {
            author_name: op_name,
            author_handle: None,
            context: None,
            text,
            allow_discord_markdown: false,
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

fn target_video_id(post_path: &str) -> Option<String> {
    let parsed = Url::parse(&ensure_absolute(post_path)).ok()?;
    if parsed.path().trim_start_matches('/').starts_with("watch") {
        if let Some(v) = parsed
            .query_pairs()
            .find(|(k, _)| k == "v")
            .map(|(_, v)| v.into_owned())
            .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()))
        {
            return Some(v);
        }
    }
    parsed
        .path_segments()?
        .rfind(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_owned)
}

fn get_video_link(
    blocks: &[Value],
    content_node: &Value,
    target_video_id: Option<&str>,
) -> Option<String> {
    video_link_in_node(content_node)
        .or_else(|| video_link_in_target_blocks(blocks, target_video_id?))
        .or_else(|| blocks.iter().find_map(video_link_in_node))
}

fn video_link_in_target_blocks(blocks: &[Value], video_id: &str) -> Option<String> {
    blocks
        .iter()
        .filter(|block| block_mentions_id(block, video_id))
        .find_map(video_link_in_node)
}

fn thumbnail_in_target_blocks(blocks: &[Value], video_id: &str) -> Option<String> {
    if video_id.is_empty() {
        return None;
    }
    blocks
        .iter()
        .filter(|block| block_mentions_id(block, video_id))
        .find_map(thumbnail_in_node)
}

fn get_op_name(blocks: &[Value], content_node: &Value, video_id: &str) -> Option<String> {
    if let Some(name) = owner_name_in_node(content_node) {
        return Some(name);
    }
    if !video_id.is_empty() {
        for bloc in blocks {
            if block_mentions_id(bloc, video_id) {
                if let Some(name) = owner_name_in_node(bloc) {
                    return Some(name);
                }
            }
        }
    }
    for bloc in blocks {
        if jq::has(bloc, &["is_additional_profile_plus"]) {
            if let Some(name) = jq::first(bloc, "owner").and_then(owner_name_from_candidate) {
                return Some(name);
            }
        }
    }
    for bloc in blocks {
        if let Some(owner) = jq::first(bloc, "owner") {
            if owner.is_object() {
                if let Some(name) = owner_name_from_candidate(owner) {
                    return Some(name);
                }
            }
        }
    }
    None
}

fn owner_name_in_node(node: &Value) -> Option<String> {
    for key in ["video_owner", "owner", "owning_profile", "owner_as_page"] {
        if let Some(owner) = node.get(key) {
            if let Some(name) = owner_name_from_candidate(owner) {
                return Some(name);
            }
        }
    }
    for key in ["video_owner", "owner", "owning_profile", "owner_as_page"] {
        for owner in jq::all(node, key) {
            if let Some(name) = owner_name_from_candidate(owner) {
                return Some(name);
            }
        }
    }
    for actors in jq::all(node, "actors") {
        if let Some(arr) = actors.as_array() {
            for actor in arr {
                if let Some(name) = owner_name_from_candidate(actor) {
                    return Some(name);
                }
            }
        }
    }
    None
}

fn owner_name_from_candidate(owner: &Value) -> Option<String> {
    if let Some(name) = val_str_at(owner, "name").filter(|s| !s.is_empty()) {
        return Some(name.to_owned());
    }
    if let Some(name) = owner
        .get("owner_as_page")
        .and_then(|v| val_str_at(v, "name"))
        .filter(|s| !s.is_empty())
    {
        return Some(name.to_owned());
    }
    None
}

fn block_mentions_id(block: &Value, needle: &str) -> bool {
    jq::all(block, "id")
        .into_iter()
        .any(|value| value_matches_id(value, needle))
}

fn value_matches_id(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(s) => s == needle,
        Value::Number(n) => n.to_string() == needle,
        _ => false,
    }
}

fn get_content_node(
    blocks: &[Value],
    html: &Html,
    raw_html: &str,
    url: &str,
    target_video_id: Option<&str>,
) -> FacebedResult<Value> {
    if let Some(video_id) = target_video_id {
        if let Some(data) = blocks.iter().find_map(|bloc| {
            result_data(bloc).filter(|data| {
                block_mentions_id(data, video_id)
                    && jq::has(
                        data,
                        &["comment_rendering_instance", "video_view_count_renderer"],
                    )
            })
        }) {
            return Ok(data.clone());
        }
    }
    for bloc in blocks {
        if jq::has(
            bloc,
            &["comment_rendering_instance", "video_view_count_renderer"],
        ) {
            if let Some(d) = jq::first(bloc, "result").and_then(|r| r.get("data")) {
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

fn result_data(block: &Value) -> Option<&Value> {
    jq::first(block, "result")?.get("data")
}

fn find_creation_time(blocks: &[Value]) -> Option<i64> {
    for bloc in blocks {
        if jq::has(bloc, &["creation_time"]) {
            let v = jq::first(bloc, "creation_time")?;
            return v
                .as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{get_content_node, get_op_name, get_video_link, target_video_id};
    use scraper::Html;
    use serde_json::json;

    #[test]
    fn op_name_prefers_content_node_owner() {
        let blocks = vec![json!({"owner": {"id": "wrong", "name": "Wrong Sidebar"}})];
        let content = json!({
            "id": "123",
            "owner": {"id": "right", "name": "Right Creator"}
        });

        assert_eq!(
            get_op_name(&blocks, &content, "123").as_deref(),
            Some("Right Creator")
        );
    }

    #[test]
    fn op_name_uses_matching_video_block_before_page_owner() {
        let content = json!({"id": "123"});
        let blocks = vec![
            json!({"owner": {"id": "wrong", "name": "Wrong Sidebar"}}),
            json!({
                "id": "123",
                "payload": {
                    "video_owner": {"id": "right", "name": "Right Creator"}
                }
            }),
        ];

        assert_eq!(
            get_op_name(&blocks, &content, "123").as_deref(),
            Some("Right Creator")
        );
    }

    #[test]
    fn op_name_uses_owner_as_page_inside_content_owner() {
        let blocks = vec![];
        let content = json!({
            "id": "123",
            "owner": {
                "__isVideoOwner": "Video",
                "id": "owner-id",
                "owner_as_page": {"id": "page-id", "name": "Right Page"}
            }
        });

        assert_eq!(
            get_op_name(&blocks, &content, "123").as_deref(),
            Some("Right Page")
        );
    }

    #[test]
    fn content_node_prefers_requested_video_over_related_feed_item() {
        let blocks = vec![
            json!({
                "result": {"data": {
                    "id": "999",
                    "feedback": {
                        "comment_rendering_instance": {},
                        "video_view_count_renderer": {}
                    }
                }}
            }),
            json!({
                "result": {"data": {
                    "id": "123",
                    "feedback": {
                        "comment_rendering_instance": {},
                        "video_view_count_renderer": {}
                    }
                }}
            }),
        ];
        let html = Html::parse_document("");
        let content = get_content_node(&blocks, &html, "", "", Some("123")).unwrap();

        assert_eq!(content.get("id").and_then(|v| v.as_str()), Some("123"));
    }

    #[test]
    fn video_link_prefers_requested_video_over_related_feed_item() {
        let content = json!({"id": "123"});
        let blocks = vec![
            json!({
                "id": "999",
                "videoDeliveryResponseFragment": {
                    "videoDeliveryResponseResult": {
                        "progressive_urls": [{"progressive_url": "https://wrong.example/video.mp4"}]
                    }
                }
            }),
            json!({
                "id": "123",
                "videoDeliveryResponseFragment": {
                    "videoDeliveryResponseResult": {
                        "progressive_urls": [{"progressive_url": "https://right.example/video.mp4"}]
                    }
                }
            }),
        ];

        assert_eq!(
            get_video_link(&blocks, &content, Some("123")).as_deref(),
            Some("https://right.example/video.mp4")
        );
    }

    #[test]
    fn target_video_id_reads_watch_query_and_page_video_path() {
        assert_eq!(
            target_video_id("watch?v=2000020650901604").as_deref(),
            Some("2000020650901604")
        );
        assert_eq!(
            target_video_id("some.page/videos/some-title/123456/").as_deref(),
            Some("123456")
        );
    }
}
