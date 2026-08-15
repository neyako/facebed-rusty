use crate::error::{FacebedError, FacebedResult};
use crate::fetch::get_json_blocks;
use crate::jq;
use crate::parsers::util::{
    author_avatar_in_node, author_handle_in_node, author_id_in_node, human_format,
    thumbnail_in_node, val_str_at, video_link_in_node,
};
use crate::parsers::{banned_post, ParsedPost, Parser, ParserCtx};
use serde_json::Value;

pub struct ReelsParser;

#[async_trait::async_trait]
impl Parser for ReelsParser {
    async fn process(&self, ctx: &ParserCtx, post_path: &str) -> FacebedResult<ParsedPost> {
        let page = ctx.fetcher.fetch(post_path, true).await?;
        let html = page.document();
        let blocks = get_json_blocks(html, true);

        let content_node = find_content_node(&blocks).ok_or_else(|| {
            FacebedError::parse_with(
                "Invalid reels link (cn)",
                page.html.clone(),
                page.url.clone(),
            )
        })?;

        let video_link = find_video_link(&blocks, &content_node).ok_or_else(|| {
            FacebedError::parse_with(
                "Invalid reels link (vn)",
                page.html.clone(),
                page.url.clone(),
            )
        })?;

        let video_id = val_str_at(&content_node, "id").unwrap_or("").to_owned();

        let owner = find_owner_with_name(&blocks, &content_node, &video_id).ok_or_else(|| {
            FacebedError::parse_with(
                "Invalid reels link (own)",
                page.html.clone(),
                page.url.clone(),
            )
        })?;
        let typename = val_str_at(&owner, "__typename").unwrap_or("");
        let is_ig = typename.starts_with("InstagramUser");
        let op_name = if is_ig {
            val_str_at(&owner, "username")
                .filter(|s| !s.is_empty())
                .map(|username| format!("📷 @{username}"))
                .unwrap_or_else(|| val_str_at(&owner, "name").unwrap_or("").to_owned())
        } else {
            val_str_at(&owner, "name").unwrap_or("").to_owned()
        };
        let owner_id = val_str_at(&owner, "id").unwrap_or("").to_owned();

        let post_url = find_shareable_url(&blocks)
            .unwrap_or_else(|| crate::url_clean::ensure_absolute(post_path));

        let date = find_creation_time(&blocks).unwrap_or(0);
        let post_text = find_message_text(&blocks);

        let (likes, cmts, shares) = get_reaction_counts(&blocks, is_ig, &video_id).unwrap_or((
            "null".into(),
            "null".into(),
            "null".into(),
        ));

        if ctx.is_banned(&owner_id) {
            return Ok(banned_post(&post_url));
        }

        let thumbnail =
            thumbnail_in_node(&content_node).or_else(|| blocks.iter().find_map(thumbnail_in_node));

        Ok(ParsedPost {
            author_name: op_name,
            author_id: author_id_in_node(&owner),
            author_handle: author_handle_in_node(&owner),
            author_avatar_url: author_avatar_in_node(&owner),
            context: None,
            text: post_text,
            allow_discord_markdown: false,
            image_links: Vec::new(),
            url: post_url,
            date,
            likes,
            comments: cmts,
            shares,
            video_links: vec![video_link],
            thumbnail,
        })
    }
}

/// Bug-1 fix: relax the selector. Old Python code required `browser_native_sd_url + creation_story`
/// in the same block, but `browser_native_sd_url` no longer exists. Match on `creation_story`
/// that has either modern (`videoDeliveryResponseFragment`) or context (`short_form_video_context`).
fn find_content_node(blocks: &[Value]) -> Option<Value> {
    for bloc in blocks {
        for cs in jq::all(bloc, "creation_story") {
            if jq::has(cs, &["short_form_video_context"])
                || jq::first(cs, "videoDeliveryResponseFragment").is_some()
                || jq::first(cs, "videoDeliveryLegacyFields").is_some()
                || jq::first(cs, "playable_url").is_some()
            {
                return Some(cs.clone());
            }
        }
    }
    None
}

fn find_video_link(blocks: &[Value], content_node: &Value) -> Option<String> {
    if let Some(link) = video_link_in_node(content_node) {
        return Some(link);
    }
    for bloc in blocks {
        if let Some(link) = video_link_in_node(bloc) {
            return Some(link);
        }
    }
    None
}

/// Prefer the owner attached to the matched video node. FB pages include
/// unrelated owner objects for sidebars, comments, and recommendations.
fn find_owner_with_name(blocks: &[Value], content_node: &Value, video_id: &str) -> Option<Value> {
    if let Some(owner) = owner_from_node(content_node) {
        return Some(owner);
    }
    if !video_id.is_empty() {
        for bloc in blocks {
            if block_mentions_id(bloc, video_id) {
                if let Some(owner) = owner_from_node(bloc) {
                    return Some(owner);
                }
            }
        }
    }
    for bloc in blocks {
        if let Some(owner) = owner_from_node(bloc) {
            return Some(owner);
        }
    }
    None
}

fn owner_from_node(node: &Value) -> Option<Value> {
    if let Some(owner) = node.pointer_path_first(&["short_form_video_context", "video_owner"]) {
        if owner_has_name(owner) {
            return Some(owner.clone());
        }
    }
    for key in ["video_owner", "owner"] {
        if let Some(owner) = node.get(key) {
            if owner_has_name(owner) {
                return Some(owner.clone());
            }
        }
    }
    for key in ["video_owner", "owner"] {
        for owner in jq::all(node, key) {
            if owner_has_name(owner) {
                return Some(owner.clone());
            }
        }
    }
    None
}

fn owner_has_name(owner: &Value) -> bool {
    owner
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false)
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

fn find_shareable_url(blocks: &[Value]) -> Option<String> {
    for bloc in blocks {
        for ctx in jq::all(bloc, "short_form_video_context") {
            if let Some(u) = ctx.get("shareable_url").and_then(|v| v.as_str()) {
                return Some(u.to_owned());
            }
        }
    }
    None
}

fn find_creation_time(blocks: &[Value]) -> Option<i64> {
    for bloc in blocks {
        for ct in jq::all(bloc, "creation_time") {
            if let Some(n) = ct.as_i64() {
                return Some(n);
            }
            if let Some(s) = ct.as_str() {
                if let Ok(n) = s.parse() {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn find_message_text(blocks: &[Value]) -> String {
    for bloc in blocks {
        for msg in jq::all(bloc, "message") {
            if let Some(t) = msg.get("text").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    return t.to_owned();
                }
            }
        }
    }
    String::new()
}

fn get_reaction_counts(
    blocks: &[Value],
    is_ig: bool,
    video_id: &str,
) -> Option<(String, String, String)> {
    let mut matched: Vec<&Value> = Vec::new();
    for bloc in blocks {
        if !jq::has(bloc, &["unified_reactors"]) {
            continue;
        }
        let ids: Vec<String> = jq::all(bloc, "id")
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .collect();
        if ids.iter().any(|x| x == video_id) {
            matched.push(bloc);
        }
    }
    if matched.is_empty() {
        return None;
    }
    let bloc = matched[0];

    let feedback = jq::all(bloc, "feedback");
    let likes = feedback
        .iter()
        .find_map(|value| {
            value
                .pointer("/unified_reactors/count")
                .or_else(|| {
                    value.pointer("/cross_universe_feedback_info/aggregated_reaction_count")
                })
                .or_else(|| value.pointer("/cross_universe_feedback_info/ig_reaction_count"))
        })
        .cloned()
        .unwrap_or(Value::Null);
    let cmts = feedback
        .iter()
        .find_map(|value| {
            if is_ig {
                value
                    .pointer("/cross_universe_feedback_info/ig_comment_count")
                    .or_else(|| value.get("total_comment_count"))
            } else {
                value.get("total_comment_count")
            }
        })
        .cloned()
        .unwrap_or(Value::Null);
    let shares = feedback
        .iter()
        .find_map(|value| value.get("share_count_reduced"))
        .cloned()
        .unwrap_or(Value::Null);

    Some((
        human_format(&likes),
        human_format(&cmts),
        human_format(&shares),
    ))
}

/// Local helper extension because `Value::pointer` only supports JSON-pointer slash paths.
trait ValueExt {
    fn pointer_path_first<'a>(&'a self, segments: &[&str]) -> Option<&'a Value>;
}

impl ValueExt for Value {
    fn pointer_path_first<'a>(&'a self, segments: &[&str]) -> Option<&'a Value> {
        // find first nested object reachable by walking `segments` as keys
        fn walk<'a>(v: &'a Value, segs: &[&str]) -> Option<&'a Value> {
            if segs.is_empty() {
                return Some(v);
            }
            match v {
                Value::Object(map) => {
                    if let Some(next) = map.get(segs[0]) {
                        if let Some(found) = walk(next, &segs[1..]) {
                            return Some(found);
                        }
                    }
                    for x in map.values() {
                        if let Some(found) = walk(x, segs) {
                            return Some(found);
                        }
                    }
                    None
                }
                Value::Array(arr) => {
                    for x in arr {
                        if let Some(found) = walk(x, segs) {
                            return Some(found);
                        }
                    }
                    None
                }
                _ => None,
            }
        }
        walk(self, segments)
    }
}

#[cfg(test)]
mod tests {
    use super::{find_owner_with_name, get_reaction_counts};
    use crate::parsers::util::{author_avatar_in_node, author_id_in_node};
    use serde_json::json;

    #[test]
    fn owner_lookup_prefers_matched_content_node() {
        let blocks = vec![json!({
            "short_form_video_context": {
                "video_owner": {"id": "wrong", "name": "Wrong Page"}
            }
        })];
        let content = json!({
            "id": "123",
            "short_form_video_context": {
                "video_owner": {
                    "id": "right",
                    "name": "Right Creator",
                    "profile_picture": {"uri": "https://img.example/reel.jpg"}
                }
            }
        });

        let owner = find_owner_with_name(&blocks, &content, "123").unwrap();

        assert_eq!(
            owner.get("name").and_then(|v| v.as_str()),
            Some("Right Creator")
        );
        assert_eq!(author_id_in_node(&owner).as_deref(), Some("right"));
        assert_eq!(
            author_avatar_in_node(&owner).as_deref(),
            Some("https://img.example/reel.jpg")
        );
    }

    #[test]
    fn owner_lookup_uses_matching_video_block_before_page_fallback() {
        let content = json!({"id": "123"});
        let blocks = vec![
            json!({"owner": {"id": "wrong", "name": "Wrong Sidebar"}}),
            json!({
                "id": "123",
                "payload": {
                    "owner": {"id": "right", "name": "Right Creator"}
                }
            }),
        ];

        let owner = find_owner_with_name(&blocks, &content, "123").unwrap();

        assert_eq!(
            owner.get("name").and_then(|v| v.as_str()),
            Some("Right Creator")
        );
    }

    #[test]
    fn reaction_counts_select_feedback_by_fields_not_tree_order() {
        let blocks = vec![json!({
            "a_nested_reaction": {
                "feedback": {
                    "cross_universe_feedback_info": {},
                    "unified_reactors": {"count": 26452}
                }
            },
            "feedback": {
                "cross_universe_feedback_info": {},
                "total_comment_count": 172,
                "share_count_reduced": "325"
            },
            "id": "video-story"
        })];

        let counts = get_reaction_counts(&blocks, false, "video-story");

        assert_eq!(counts, Some(("26.452".into(), "172".into(), "325".into())));
    }
}
