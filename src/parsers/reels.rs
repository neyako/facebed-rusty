use crate::error::{FacebedError, FacebedResult};
use crate::fetch::get_json_blocks;
use crate::jq;
use crate::parsers::util::{
    author_avatar_in_node, author_handle_in_node, author_id_in_node, human_format,
    thumbnail_in_node, val_str_at,
};
use crate::parsers::{banned_post, ParsedPost, Parser, ParserCtx};
use serde_json::Value;

pub struct ReelsParser;

struct SelectedContentNode {
    media: Value,
    context: Value,
    video_id: String,
}

#[async_trait::async_trait]
impl Parser for ReelsParser {
    async fn process(&self, ctx: &ParserCtx, post_path: &str) -> FacebedResult<ParsedPost> {
        let page = ctx.fetcher.fetch(post_path, true).await?;
        let html = page.document();
        let blocks = get_json_blocks(html, true);

        let target_video_id = reel_id_from_path(post_path);
        let selected = select_content_node(&blocks, target_video_id).ok_or_else(|| {
            FacebedError::parse_with(
                "Invalid reels link (cn)",
                page.html.clone(),
                page.url.clone(),
            )
        })?;

        let video_id = selected.video_id.as_str();
        let video_link = find_video_link(&blocks, &selected.media, video_id).ok_or_else(|| {
            FacebedError::parse_with(
                "Invalid reels link (vn)",
                page.html.clone(),
                page.url.clone(),
            )
        })?;

        let owner =
            find_owner_with_name(&blocks, &selected.context, video_id).ok_or_else(|| {
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
        let owner_id = owner_id_for_post(&owner).ok_or_else(|| {
            FacebedError::parse_with(
                "Invalid reels link (own)",
                page.html.clone(),
                page.url.clone(),
            )
        })?;

        let post_url = find_shareable_url(&blocks)
            .unwrap_or_else(|| crate::url_clean::ensure_absolute(post_path));

        let date = find_creation_time(&blocks).unwrap_or(0);
        let post_text = find_message_text(&blocks, &selected.context, video_id);

        let (likes, cmts, shares) = get_reaction_counts(&blocks, is_ig, video_id).unwrap_or((
            "null".into(),
            "null".into(),
            "null".into(),
        ));

        if ctx.is_banned(&owner_id) {
            return Ok(banned_post(&post_url));
        }

        let thumbnail =
            thumbnail_in_node(&selected.media).or_else(|| thumbnail_in_node(&selected.context));

        Ok(ParsedPost {
            author_name: op_name,
            author_id: Some(owner_id),
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
fn select_content_node(
    blocks: &[Value],
    target_video_id: Option<&str>,
) -> Option<SelectedContentNode> {
    if let Some(target_video_id) = target_video_id {
        for bloc in blocks {
            for cs in jq::all(bloc, "creation_story") {
                if is_strict_content_story(cs)
                    && cs
                        .get("id")
                        .is_some_and(|id| value_matches_id(id, target_video_id))
                {
                    return Some(selected_content(
                        cs.clone(),
                        cs.clone(),
                        Some(target_video_id),
                    ));
                }
            }
        }
    } else {
        for bloc in blocks {
            for cs in jq::all(bloc, "creation_story") {
                if is_strict_content_story(cs) {
                    return Some(selected_content(cs.clone(), cs.clone(), None));
                }
            }
        }
    }
    if let Some(target_video_id) = target_video_id {
        for bloc in blocks {
            if let Some((media, context)) = find_delivery_fallback_candidate(bloc, target_video_id)
            {
                return Some(selected_content(media, context, Some(target_video_id)));
            }
        }
    }
    None
}

fn selected_content(
    media: Value,
    context: Value,
    target_video_id: Option<&str>,
) -> SelectedContentNode {
    let video_id = target_video_id
        .map(ToOwned::to_owned)
        .or_else(|| val_str_at(&media, "id").map(ToOwned::to_owned))
        .unwrap_or_default();
    SelectedContentNode {
        media,
        context,
        video_id,
    }
}

fn is_strict_content_story(candidate: &Value) -> bool {
    jq::has(candidate, &["short_form_video_context"])
        || jq::first(candidate, "videoDeliveryResponseFragment").is_some()
        || jq::first(candidate, "videoDeliveryLegacyFields").is_some()
        || jq::first(candidate, "playable_url").is_some()
}

fn find_delivery_fallback_candidate(node: &Value, target_video_id: &str) -> Option<(Value, Value)> {
    match node {
        Value::Object(map) => {
            if matches_target_delivery(node, target_video_id) && has_comment_renderer(node) {
                return Some((node.clone(), node.clone()));
            }
            if has_target_media_context(node) {
                if let Some(attachments) = node.get("attachments").and_then(Value::as_array) {
                    for attachment in attachments {
                        let Some(media) = attachment.get("media") else {
                            continue;
                        };
                        if !media.is_object() || !matches_target_delivery(media, target_video_id) {
                            continue;
                        }
                        let context = if has_comment_renderer(media) {
                            media.clone()
                        } else {
                            node.clone()
                        };
                        return Some((media.clone(), context));
                    }
                }
            }
            map.values()
                .find_map(|child| find_delivery_fallback_candidate(child, target_video_id))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|child| find_delivery_fallback_candidate(child, target_video_id)),
        _ => None,
    }
}

fn has_target_media_context(node: &Value) -> bool {
    node.get("message").is_some()
        && node
            .get("actors")
            .and_then(Value::as_array)
            .and_then(|actors| actors.first())
            .is_some_and(owner_has_name)
}

fn matches_target_delivery(candidate: &Value, target_video_id: &str) -> bool {
    candidate
        .get("id")
        .is_some_and(|id| value_matches_id(id, target_video_id))
        && candidate.get("videoDeliveryResponseFragment").is_some()
}

fn has_comment_renderer(candidate: &Value) -> bool {
    candidate.get("comment_rendering_instance").is_some()
        || candidate
            .get("feedback")
            .and_then(|feedback| feedback.get("comment_rendering_instance"))
            .is_some()
}

fn reel_id_from_path(post_path: &str) -> Option<&str> {
    let path = post_path
        .split_once('?')
        .map_or(post_path, |(path, _)| path)
        .trim_start_matches('/');
    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    if segments.next()? != "reel" {
        return None;
    }
    let id = segments.next()?;
    if segments.next().is_some() || !id.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(id)
}

fn find_video_link(
    blocks: &[Value],
    content_node: &Value,
    target_video_id: &str,
) -> Option<String> {
    if let Some(link) = direct_video_link(content_node) {
        return Some(link);
    }
    if target_video_id.is_empty() {
        return None;
    }
    for bloc in blocks {
        if let Some(link) = find_target_video_link(bloc, target_video_id) {
            return Some(link);
        }
    }
    None
}

fn find_target_video_link(node: &Value, target_video_id: &str) -> Option<String> {
    match node {
        Value::Object(map) => {
            if node
                .get("id")
                .is_some_and(|id| value_matches_id(id, target_video_id))
            {
                if let Some(link) = direct_video_link(node) {
                    return Some(link);
                }
            }
            map.values()
                .find_map(|child| find_target_video_link(child, target_video_id))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|child| find_target_video_link(child, target_video_id)),
        _ => None,
    }
}

fn direct_video_link(node: &Value) -> Option<String> {
    if let Some(progressive_urls) = node
        .get("videoDeliveryResponseFragment")
        .and_then(|fragment| fragment.get("videoDeliveryResponseResult"))
        .and_then(|result| result.get("progressive_urls"))
        .and_then(Value::as_array)
    {
        for entry in progressive_urls {
            if let Some(url) = entry
                .get("progressive_url")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                return Some(url.to_owned());
            }
        }
    }
    if let Some(legacy) = node.get("videoDeliveryLegacyFields") {
        for key in ["browser_native_hd_url", "browser_native_sd_url"] {
            if let Some(url) = legacy
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                return Some(url.to_owned());
            }
        }
    }
    for key in ["playable_url_quality_hd", "playable_url"] {
        if let Some(url) = node
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            return Some(url.to_owned());
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
            if let Some(owner) = owner_linked_to_id(bloc, video_id) {
                return Some(owner);
            }
        }
    }
    None
}

fn owner_linked_to_id(node: &Value, video_id: &str) -> Option<Value> {
    match node {
        Value::Object(map) => {
            if node
                .get("id")
                .is_some_and(|id| value_matches_id(id, video_id))
            {
                if let Some(owner) = owner_from_node(node) {
                    return Some(owner);
                }
            }
            map.values()
                .find_map(|child| owner_linked_to_id(child, video_id))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|child| owner_linked_to_id(child, video_id)),
        _ => None,
    }
}

fn owner_from_node(node: &Value) -> Option<Value> {
    let bound_context = has_target_media_context(node);
    if bound_context {
        return node
            .get("actors")
            .and_then(Value::as_array)
            .and_then(|actors| actors.first())
            .cloned();
    }
    if let Some(owner) = node
        .get("short_form_video_context")
        .and_then(|context| context.get("video_owner"))
    {
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
    if let Some(owner) = node.pointer_path_first(&["short_form_video_context", "video_owner"]) {
        if owner_has_name(owner) {
            return Some(owner.clone());
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
        && author_id_in_node(owner).is_some_and(|id| !id.is_empty())
}

fn owner_id_for_post(owner: &Value) -> Option<String> {
    author_id_in_node(owner).filter(|id| !id.is_empty())
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

fn find_message_text(blocks: &[Value], content_node: &Value, video_id: &str) -> String {
    if has_target_media_context(content_node) {
        return direct_message_text(content_node).unwrap_or_default();
    }
    if let Some(text) = first_message_text(content_node) {
        return text;
    }
    if !video_id.is_empty() {
        for block in blocks {
            if let Some(text) = linked_message_text(block, video_id) {
                return text;
            }
        }
        return String::new();
    }
    for bloc in blocks {
        if let Some(text) = first_message_text(bloc) {
            return text;
        }
    }
    String::new()
}

fn first_message_text(node: &Value) -> Option<String> {
    if let Some(text) = direct_message_text(node) {
        return Some(text);
    }
    jq::all(node, "message")
        .into_iter()
        .find_map(|message| message_text(message).filter(|text| !text.is_empty()))
}

fn direct_message_text(node: &Value) -> Option<String> {
    for key in ["message", "message_preferred_body"] {
        if let Some(text) = node
            .get(key)
            .and_then(message_text)
            .filter(|text| !text.is_empty())
        {
            return Some(text);
        }
    }
    None
}

fn message_text(message: &Value) -> Option<String> {
    message
        .get("text")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn linked_message_text(node: &Value, video_id: &str) -> Option<String> {
    match node {
        Value::Object(map) => {
            let linked = ["id", "video_id", "videoId", "videoID"]
                .iter()
                .filter_map(|key| map.get(*key))
                .any(|value| value_matches_id(value, video_id));
            if linked {
                if let Some(text) = first_message_text(node) {
                    return Some(text);
                }
            }
            map.values()
                .find_map(|child| linked_message_text(child, video_id))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|child| linked_message_text(child, video_id)),
        _ => None,
    }
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
    use super::{
        find_message_text, find_owner_with_name, find_video_link, get_reaction_counts,
        owner_has_name, owner_id_for_post, reel_id_from_path, select_content_node,
    };
    use crate::parsers::util::{author_avatar_in_node, author_id_in_node, val_str_at};
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

    #[test]
    fn caption_lookup_does_not_return_unrelated_video_message() {
        let content_node = json!({"id": "focal-video"});
        let blocks = vec![
            json!({
                "id": "sidebar-video",
                "message": {"text": "DECOY CAPTION"}
            }),
            json!({
                "id": "focal-video",
                "message": {"text": "FOCAL CAPTION"}
            }),
        ];

        assert_eq!(
            find_message_text(&blocks, &content_node, "focal-video"),
            "FOCAL CAPTION"
        );
    }

    #[test]
    fn caption_lookup_keeps_legacy_fallback_without_video_link() {
        let blocks = vec![json!({"message": {"text": "LEGACY CAPTION"}})];

        assert_eq!(find_message_text(&blocks, &json!({}), ""), "LEGACY CAPTION");
    }

    #[test]
    fn video_link_prefers_target_associated_url_over_decoy() {
        let content = json!({"id": "target-video"});
        let blocks = vec![
            json!({"id": "decoy-video", "playable_url": "https://video.example/decoy.mp4"}),
            json!({"id": "target-video", "playable_url": "https://video.example/target.mp4"}),
        ];

        assert_eq!(
            find_video_link(&blocks, &content, "target-video"),
            Some("https://video.example/target.mp4".to_string())
        );
    }

    #[test]
    fn video_link_fails_closed_when_only_decoy_exists() {
        let content = json!({"id": "target-video"});
        let blocks = vec![json!({
            "id": "decoy-video",
            "playable_url": "https://video.example/decoy.mp4"
        })];

        assert_eq!(find_video_link(&blocks, &content, "target-video"), None);
    }

    #[test]
    fn video_link_prefers_selected_content_node_local_url() {
        let content = json!({
            "id": "target-video",
            "playable_url": "https://video.example/local.mp4"
        });
        let blocks = vec![json!({
            "id": "decoy-video",
            "playable_url": "https://video.example/decoy.mp4"
        })];

        assert_eq!(
            find_video_link(&blocks, &content, "target-video"),
            Some("https://video.example/local.mp4".to_string())
        );
    }

    #[test]
    fn video_link_rejects_nested_decoy_below_target_wrapper() {
        let content = json!({
            "id": "target-video",
            "related": {"id": "decoy-video", "playable_url": "https://video.example/decoy.mp4"}
        });

        assert_eq!(find_video_link(&[], &content, "target-video"), None);
    }

    #[test]
    fn video_link_prefers_direct_target_delivery_over_nested_decoy() {
        let content = json!({
            "id": "target-video",
            "playable_url": "https://video.example/target.mp4",
            "related": {"id": "decoy-video", "playable_url": "https://video.example/decoy.mp4"}
        });

        assert_eq!(
            find_video_link(&[], &content, "target-video"),
            Some("https://video.example/target.mp4".to_string())
        );
    }

    #[test]
    fn video_link_finds_target_child_inside_unrelated_outer_block() {
        let blocks = vec![json!({
            "unrelated": {"playable_url": "https://video.example/decoy.mp4"},
            "target": {"id": "target-video", "playable_url": "https://video.example/target.mp4"}
        })];

        assert_eq!(
            find_video_link(&blocks, &json!({}), "target-video"),
            Some("https://video.example/target.mp4".to_string())
        );
    }

    #[test]
    fn video_link_rejects_nested_progressive_url_inside_target_fragment() {
        let content = json!({
            "id": "target-video",
            "videoDeliveryResponseFragment": {
                "sidebar": {"progressive_url": "https://video.example/decoy.mp4"}
            }
        });

        assert_eq!(find_video_link(&[], &content, "target-video"), None);
    }

    #[test]
    fn video_link_reads_exact_progressive_urls_and_ignores_fragment_siblings() {
        let content = json!({
            "id": "target-video",
            "videoDeliveryResponseFragment": {
                "sidebar": {"progressive_url": "https://video.example/decoy.mp4"},
                "videoDeliveryResponseResult": {
                    "progressive_urls": [
                        {"progressive_url": "https://video.example/target.mp4"}
                    ],
                    "related": {"progressive_url": "https://video.example/decoy-2.mp4"}
                }
            }
        });

        assert_eq!(
            find_video_link(&[], &content, "target-video"),
            Some("https://video.example/target.mp4".to_string())
        );
    }

    #[test]
    fn video_link_reads_direct_legacy_hd_url() {
        let content = json!({
            "id": "target-video",
            "videoDeliveryLegacyFields": {"browser_native_hd_url": "https://video.example/hd.mp4"}
        });

        assert_eq!(
            find_video_link(&[], &content, "target-video"),
            Some("https://video.example/hd.mp4".to_string())
        );
    }

    #[test]
    fn caption_lookup_does_not_fallback_to_decoy_when_focal_message_is_missing() {
        let blocks = vec![json!({
            "id": "sidebar-video",
            "message": {"text": "DECOY CAPTION"}
        })];

        assert_eq!(
            find_message_text(&blocks, &json!({"id": "focal-video"}), "focal-video"),
            ""
        );
    }

    #[test]
    fn content_node_accepts_same_candidate_comment_and_delivery_fields() {
        let blocks = vec![json!({
            "id": "1376968477584004",
            "comment_rendering_instance": {},
            "videoDeliveryResponseFragment": {
                "videoDeliveryResponseResult": {
                    "progressive_urls": [
                        {"progressive_url": "https://video.example/reel.mp4"}
                    ]
                }
            }
        })];

        let content = select_content_node(&blocks, Some("1376968477584004"))
            .expect("fallback candidate")
            .media;

        assert_eq!(
            content.get("id").and_then(|v| v.as_str()),
            Some("1376968477584004")
        );
    }

    #[test]
    fn content_node_accepts_nested_feedback_comment_renderer() {
        let blocks = vec![json!({
            "id": "1013234327723021",
            "feedback": {"comment_rendering_instance": {}},
            "videoDeliveryResponseFragment": {}
        })];

        let content = select_content_node(&blocks, Some("1013234327723021"))
            .expect("nested fallback candidate")
            .media;

        assert_eq!(
            content.get("id").and_then(|v| v.as_str()),
            Some("1013234327723021")
        );
    }

    #[test]
    fn content_node_prefers_target_strict_story_over_qualified_decoy() {
        let blocks = vec![json!({
            "creation_story": {
                "id": "9999999999999999",
                "short_form_video_context": {},
                "videoDeliveryResponseFragment": {}
            },
            "other": {
                "creation_story": {
                    "id": "1013234327723021",
                    "short_form_video_context": {},
                    "videoDeliveryResponseFragment": {}
                }
            }
        })];

        let content = select_content_node(&blocks, Some("1013234327723021"))
            .expect("target strict story must be selected")
            .media;

        assert_eq!(
            content.get("id").and_then(|v| v.as_str()),
            Some("1013234327723021")
        );
    }

    #[test]
    fn content_node_uses_target_fallback_when_strict_story_is_decoy() {
        let blocks = vec![json!({
            "creation_story": {
                "id": "9999999999999999",
                "short_form_video_context": {},
                "videoDeliveryResponseFragment": {}
            },
            "id": "1013234327723021",
            "feedback": {"comment_rendering_instance": {}},
            "videoDeliveryResponseFragment": {}
        })];

        let content = select_content_node(&blocks, Some("1013234327723021"))
            .expect("target fallback must be selected")
            .media;

        assert_eq!(
            content.get("id").and_then(|v| v.as_str()),
            Some("1013234327723021")
        );
    }

    #[test]
    fn content_node_keeps_first_strict_story_without_target_id() {
        let blocks = vec![json!({
            "creation_story": {
                "id": "9999999999999999",
                "short_form_video_context": {},
                "videoDeliveryResponseFragment": {}
            },
            "other": {
                "creation_story": {
                    "id": "1013234327723021",
                    "short_form_video_context": {},
                    "videoDeliveryResponseFragment": {}
                }
            }
        })];

        let content = select_content_node(&blocks, None)
            .expect("legacy strict story must be selected")
            .media;

        assert_eq!(
            content.get("id").and_then(|v| v.as_str()),
            Some("9999999999999999")
        );
    }

    #[test]
    fn content_node_prefers_requested_target_over_qualified_decoy() {
        let blocks = vec![
            json!({
                "id": "9999999999999999",
                "comment_rendering_instance": {},
                "videoDeliveryResponseFragment": {}
            }),
            json!({
                "id": "1013234327723021",
                "comment_rendering_instance": {},
                "videoDeliveryResponseFragment": {}
            }),
        ];

        let content = select_content_node(&blocks, Some("1013234327723021"))
            .expect("target fallback candidate")
            .media;

        assert_eq!(
            content.get("id").and_then(|v| v.as_str()),
            Some("1013234327723021")
        );
    }

    #[test]
    fn content_node_rejects_qualified_candidate_when_requested_target_is_absent() {
        let blocks = vec![json!({
            "id": "9999999999999999",
            "comment_rendering_instance": {},
            "videoDeliveryResponseFragment": {}
        })];

        assert!(select_content_node(&blocks, Some("1013234327723021")).is_none());
    }

    #[test]
    fn content_node_rejects_unanchored_delivery_wrapper_without_comment_renderer() {
        let blocks = vec![json!({
            "id": "9999999999999999",
            "videoDeliveryResponseFragment": {
                "videoDeliveryResponseResult": {
                    "progressive_urls": [
                        {"progressive_url": "https://video.example/sidebar.mp4"}
                    ]
                }
            }
        })];

        assert!(select_content_node(&blocks, Some("9999999999999999")).is_none());
    }

    #[test]
    fn content_node_accepts_target_media_node_without_comment_renderer() {
        let blocks = vec![json!({
            "creation_story": {"id": "UzpfSTEwMDA2NDgwMDc4Mzk="},
            "message": {"text": "TARGET CAPTION"},
            "actors": [{"id": "target-owner", "name": "Target Owner"}],
            "attachments": [{
                "media": {
                    "id": "1013234327723021",
                    "videoDeliveryResponseFragment": {
                        "videoDeliveryResponseResult": {
                            "progressive_urls": [
                                {"progressive_url": "https://video.example/reel.mp4"}
                            ]
                        }
                    },
                    "videoDeliveryLegacyFields": {}
                }
            }]
        })];

        let selected = select_content_node(&blocks, Some("1013234327723021"))
            .expect("target media node must be selected without comment renderer");

        assert_eq!(
            selected
                .context
                .get("message")
                .and_then(|value| value.get("text")),
            Some(&json!("TARGET CAPTION"))
        );
    }

    #[test]
    fn target_media_provenance_binds_caption_and_owner_away_from_decoy_story() {
        let blocks = vec![json!({
            "creation_story": {
                "owner": {"id": "decoy-owner", "name": "Decoy Owner"},
                "message": {"text": "DECOY CAPTION"}
            },
            "message": {
                "message": {"text": "TARGET CAPTION"},
                "owner": {"id": "decoy-direct-owner", "name": "Decoy Direct Owner"},
                "actors": [{"id": "target-owner", "name": "Target Owner"}],
                "feedback": {"id": "feedback-target"},
                "attachments": [{
                    "media": {
                        "id": "1013234327723021",
                        "videoDeliveryResponseFragment": {
                            "videoDeliveryResponseResult": {
                                "progressive_urls": [
                                    {"progressive_url": "https://video.example/reel.mp4"}
                                ]
                            }
                        },
                        "videoDeliveryLegacyFields": {}
                    }
                }]
            }
        })];

        let selected = select_content_node(&blocks, Some("1013234327723021"))
            .expect("target media provenance must select enclosing message");
        let owner = find_owner_with_name(&blocks, &selected.context, &selected.video_id)
            .expect("target actor owner");

        assert_eq!(selected.video_id, "1013234327723021");
        assert_eq!(
            find_video_link(&blocks, &selected.media, &selected.video_id),
            Some("https://video.example/reel.mp4".to_owned())
        );
        assert_eq!(
            find_message_text(&blocks, &selected.context, &selected.video_id),
            "TARGET CAPTION"
        );
        assert_eq!(
            owner.get("id").and_then(|value| value.as_str()),
            Some("target-owner")
        );
        assert_ne!(
            owner.get("id").and_then(|value| value.as_str()),
            Some("decoy-owner")
        );
    }

    #[test]
    fn owner_lookup_fails_closed_without_target_link() {
        let blocks = vec![json!({
            "owner": {"id": "decoy-owner", "name": "Decoy Owner"},
            "related": {"id": "1013234327723021"}
        })];

        assert!(find_owner_with_name(
            &blocks,
            &json!({"id": "1013234327723021"}),
            "1013234327723021"
        )
        .is_none());
    }

    #[test]
    fn bound_context_prefers_direct_actor_and_message_over_nested_decoys() {
        let content = json!({
            "aaa_decoy": {
                "actors": [{"id": "decoy-owner", "name": "Decoy Owner"}],
                "message": {"text": "DECOY CAPTION"}
            },
            "owner": {"id": "decoy-direct-owner", "name": "Decoy Direct Owner"},
            "message": {"text": "TARGET CAPTION"},
            "actors": [
                {"id": "target-owner", "name": "Target Owner"},
                {"id": "decoy-second-owner", "name": "Decoy Second Owner"}
            ]
        });

        let owner =
            find_owner_with_name(&[], &content, "1013234327723021").expect("direct target actor");

        assert_eq!(
            find_message_text(&[], &content, "1013234327723021"),
            "TARGET CAPTION"
        );
        assert_eq!(
            owner.get("id").and_then(|value| value.as_str()),
            Some("target-owner")
        );
    }

    #[test]
    fn fallback_requires_attachments_array_media_provenance() {
        let blocks = vec![json!({
            "message": {
                "actors": [{"id": "target-owner", "name": "Target Owner"}],
                "attachments": {
                    "media": {
                        "id": "1013234327723021",
                        "videoDeliveryResponseFragment": {}
                    }
                }
            }
        })];

        assert!(select_content_node(&blocks, Some("1013234327723021")).is_none());
    }

    #[test]
    fn bound_context_rejects_invalid_first_actor_even_when_second_is_valid() {
        let content = json!({
            "message": {"text": "TARGET CAPTION"},
            "actors": [
                {"name": "Missing ID Actor"},
                {"id": "target-owner", "name": "Target Owner"}
            ]
        });

        assert!(find_owner_with_name(&[], &content, "1013234327723021").is_none());
    }

    #[test]
    fn numeric_actor_id_is_normalized_for_owner_ban_lookup() {
        let owner = json!({"id": 12345, "name": "Numeric Owner"});

        assert_eq!(owner_id_for_post(&owner).as_deref(), Some("12345"));
        assert!(owner_has_name(&owner));
    }

    #[test]
    fn renderer_candidate_keeps_own_context_inside_unrelated_bound_message() {
        let blocks = vec![json!({
            "message": {
                "message": {"text": "DECOY CAPTION"},
                "actors": [{"id": "decoy-owner", "name": "Decoy Owner"}],
                "attachments": [{
                    "media": {
                        "id": "1013234327723021",
                        "videoDeliveryResponseFragment": {},
                        "comment_rendering_instance": {}
                    }
                }]
            }
        })];

        let selected = select_content_node(&blocks, Some("1013234327723021"))
            .expect("renderer-qualified candidate");

        assert_eq!(selected.context, selected.media);
        assert!(selected.context.get("actors").is_none());
    }

    #[test]
    fn nested_or_array_media_provenance_is_rejected() {
        let nested_attachments = vec![json!({
            "message": {"text": "DECOY CAPTION"},
            "actors": [{"id": "decoy-owner", "name": "Decoy Owner"}],
            "foo": {
                "attachments": [{
                    "media": {
                        "id": "1013234327723021",
                        "videoDeliveryResponseFragment": {}
                    }
                }]
            }
        })];
        let array_media = vec![json!({
            "message": {"text": "DECOY CAPTION"},
            "actors": [{"id": "decoy-owner", "name": "Decoy Owner"}],
            "attachments": [{
                "media": [{
                    "id": "1013234327723021",
                    "videoDeliveryResponseFragment": {}
                }]
            }]
        })];

        assert!(select_content_node(&nested_attachments, Some("1013234327723021")).is_none());
        assert!(select_content_node(&array_media, Some("1013234327723021")).is_none());
    }

    #[test]
    fn process_chain_uses_route_target_id_for_video_link() {
        let blocks = vec![json!({
            "message": {
                "id": "UzpfSTEwMDA2NDgwMDc4Mzk=",
                "message": {"text": "TARGET CAPTION"},
                "actors": [{"id": "target-owner", "name": "Target Owner"}],
                "attachments": [{
                    "media": {
                        "id": "1013234327723021",
                        "videoDeliveryResponseFragment": {}
                    }
                }]
            },
            "target_delivery": {
                "id": "1013234327723021",
                "playable_url": "https://video.example/reel.mp4"
            }
        })];

        let selected = select_content_node(&blocks, Some("1013234327723021"))
            .expect("target media provenance");
        let old_context_video_id = val_str_at(&selected.context, "id").unwrap_or("");

        assert_eq!(old_context_video_id, "UzpfSTEwMDA2NDgwMDc4Mzk=");
        assert_eq!(
            find_video_link(&blocks, &selected.media, old_context_video_id),
            None,
            "old context-derived ID reproduces Invalid reels link (vn)"
        );
        assert_eq!(selected.video_id, "1013234327723021");
        assert_eq!(
            find_video_link(&blocks, &selected.media, &selected.video_id),
            Some("https://video.example/reel.mp4".to_owned())
        );
    }

    #[test]
    fn reel_id_from_path_uses_final_numeric_segment() {
        assert_eq!(
            reel_id_from_path("reel/1013234327723021?mibextid=abc"),
            Some("1013234327723021")
        );
        assert_eq!(
            reel_id_from_path("reel/1013234327723021/"),
            Some("1013234327723021")
        );
        assert_eq!(reel_id_from_path("reel/1/2"), None);
        assert_eq!(reel_id_from_path("reel/1/2/3"), None);
        assert_eq!(reel_id_from_path("prefix/reel/1"), None);
        assert_eq!(reel_id_from_path("reel/not-a-number"), None);
    }
}
