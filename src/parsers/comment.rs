use crate::error::{FacebedError, FacebedResult};
use crate::fetch::get_json_blocks;
use crate::jq;
use crate::parsers::util::{
    human_format, images_from_post, thumbnail_in_node, val_str_at, video_link_in_node,
    videos_from_post,
};
use crate::parsers::{banned_post, ParsedPost, Parser, ParserCtx};
use crate::url_clean::ensure_absolute;
use serde_json::Value;
use url::Url;

pub struct CommentParser;

/// Base64 (standard or URL-safe alphabet) → ASCII string. FB comment `id`
/// fields are base64 of `comment:<post_fbid>_<comment_fbid>`.
/// ponytail: hand-rolled to avoid a base64 crate for one call site.
fn b64_decode_ascii(s: &str) -> Option<String> {
    let mut bits: u32 = 0;
    let mut n = 0u32;
    let mut out = Vec::new();
    for &c in s.trim_end_matches('=').as_bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32;
        bits = (bits << 6) | v;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
        }
    }
    String::from_utf8(out).ok()
}

/// The `comment_id` query value, if present and non-empty.
pub(crate) fn comment_id_in(path: &str) -> Option<String> {
    let url = Url::parse(&ensure_absolute(path)).ok()?;
    url.query_pairs()
        .find(|(k, _)| k == "comment_id")
        .map(|(_, v)| v.into_owned())
        .filter(|v| !v.is_empty())
}

/// Any object with a comment body and an author is a candidate comment node.
fn candidate_comment_nodes(bloc: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    for edges in jq::all(bloc, "edges") {
        let Some(arr) = edges.as_array() else {
            continue;
        };
        for edge in arr {
            if let Some(node) = edge.get("node") {
                if node.get("preferred_body").is_some() && node.get("author").is_some() {
                    out.push(node);
                }
            }
        }
    }
    // Single highlighted-comment shapes outside edges lists.
    for key in ["comment", "attached_comment"] {
        for node in jq::all(bloc, key) {
            if node.get("preferred_body").is_some() && node.get("author").is_some() {
                out.push(node);
            }
        }
    }
    out
}

fn node_matches_id(node: &Value, comment_id: &str) -> bool {
    match node.get("legacy_fbid") {
        Some(Value::String(s)) if s == comment_id => return true,
        Some(Value::Number(n)) if n.to_string() == comment_id => return true,
        _ => {}
    }
    let needle = format!("comment_id={comment_id}");
    for url_val in jq::all(node, "url") {
        if url_val.as_str().is_some_and(|s| s.contains(&needle)) {
            return true;
        }
    }
    if let Some(id) = node.get("id").and_then(|v| v.as_str()) {
        if let Some(decoded) = b64_decode_ascii(id) {
            if decoded.starts_with("comment:") && decoded.ends_with(&format!("_{comment_id}")) {
                return true;
            }
        }
    }
    false
}

fn find_comment_node<'a>(blocks: &'a [Value], comment_id: &str) -> Option<&'a Value> {
    blocks
        .iter()
        .flat_map(|b| candidate_comment_nodes(b))
        .find(|n| node_matches_id(n, comment_id))
}

/// Comment permalink URL: the node's own feedback URL when FB provides one
/// (it points at the comment, not just the post), else the request URL.
fn comment_url(node: &Value, post_path: &str) -> String {
    for url_val in jq::all(node, "url") {
        if let Some(s) = url_val.as_str() {
            if s.starts_with("https://") && s.contains("comment_id=") {
                return s.to_owned();
            }
        }
    }
    ensure_absolute(post_path)
}

fn comment_reactions(node: &Value) -> Value {
    for key in ["reactors", "unified_reactors"] {
        if let Some(count) = jq::first(node, key).and_then(|r| r.get("count")) {
            return count.clone();
        }
    }
    Value::Null
}

fn parsed_post_from_comment(node: &Value, post_path: &str) -> FacebedResult<ParsedPost> {
    let author = node
        .get("author")
        .ok_or_else(|| FacebedError::parse("comment author missing (cau)"))?;
    let author_name = val_str_at(author, "name")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| FacebedError::parse("comment author name missing (cau)"))?;
    let text = node
        .pointer("/preferred_body/text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let date = node
        .get("created_time")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let image_links = images_from_post(node);
    let mut video_links = videos_from_post(node);
    if video_links.is_empty() {
        if let Some(link) = video_link_in_node(node) {
            video_links.push(link);
        }
    }
    let thumbnail = if video_links.is_empty() {
        None
    } else {
        jq::all(node, "media")
            .into_iter()
            .find_map(thumbnail_in_node)
            .or_else(|| thumbnail_in_node(node))
    };

    Ok(ParsedPost {
        author_name: format!("{author_name} (💬)"),
        text,
        allow_discord_markdown: false,
        image_links,
        url: comment_url(node, post_path),
        date,
        likes: human_format(&comment_reactions(node)),
        comments: "null".into(),
        shares: "null".into(),
        video_links,
        thumbnail,
    })
}

#[async_trait::async_trait]
impl Parser for CommentParser {
    async fn process(&self, ctx: &ParserCtx, post_path: &str) -> FacebedResult<ParsedPost> {
        let comment_id = comment_id_in(post_path)
            .ok_or_else(|| FacebedError::parse("comment path without comment_id"))?;
        let page = ctx.fetcher.fetch(post_path, true).await?;
        let blocks = get_json_blocks(page.document(), true);
        let Some(node) = find_comment_node(&blocks, &comment_id) else {
            // Deep replies / stale ids aren't server-rendered. NoData (not
            // Parse) so routes falls back to the plain post embed. (ccn)
            return Err(FacebedError::no_data(format!(
                "comment {comment_id} not in server-rendered HTML (ccn)"
            )));
        };
        if let Some(author_id) = node.pointer("/author/id").and_then(|v| v.as_str()) {
            if ctx.is_banned(author_id) {
                return Ok(banned_post(&ensure_absolute(post_path)));
            }
        }
        parsed_post_from_comment(node, post_path).map_err(|e| match e {
            FacebedError::Parse { message, .. } => {
                FacebedError::parse_with(message, page.html.clone(), page.url.clone())
            }
            other => other,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn comment_block() -> Value {
        json!({
            "comment_rendering_instance": {
                "comments": {
                    "edges": [
                        {"node": {
                            "id": "Y29tbWVudDo5OTlfMTEx",           // base64("comment:999_111")
                            "legacy_fbid": "111",
                            "author": {"name": "Alice", "id": "42"},
                            "preferred_body": {"text": "first!"},
                            "created_time": 1750000000,
                            "feedback": {"url": "https://www.facebook.com/x?comment_id=111"},
                            "reactors": {"count": 7}
                        }},
                        {"node": {
                            "id": "Y29tbWVudDo5OTlfMjIy",           // base64("comment:999_222")
                            "author": {"name": "Bob", "id": "43"},
                            "preferred_body": {"text": "video reply"},
                            "created_time": 1750000100,
                            "attachments": [{"style_type_renderer": {"attachment": {"media": {
                                "videoDeliveryResponseFragment": {
                                    "videoDeliveryResponseResult": {
                                        "progressive_urls": [
                                            {"progressive_url": "https://video.fbcdn.net/c.mp4"}
                                        ]
                                    }
                                },
                                "preferred_thumbnail": {"image": {"uri": "https://img.fbcdn.net/t.jpg"}}
                            }}}}]
                        }}
                    ]
                }
            }
        })
    }

    #[test]
    fn b64_decodes_comment_ids() {
        assert_eq!(
            b64_decode_ascii("Y29tbWVudDo5OTlfMTEx").as_deref(),
            Some("comment:999_111")
        );
        assert_eq!(b64_decode_ascii("!!!"), None);
    }

    #[test]
    fn comment_id_in_reads_query() {
        assert_eq!(
            comment_id_in("reel/999?comment_id=111").as_deref(),
            Some("111")
        );
        assert_eq!(comment_id_in("reel/999"), None);
        assert_eq!(comment_id_in("reel/999?comment_id="), None);
    }

    #[test]
    fn finds_comment_by_legacy_fbid() {
        let blocks = vec![comment_block()];
        let node = find_comment_node(&blocks, "111").unwrap();
        assert_eq!(
            node.pointer("/preferred_body/text")
                .and_then(|v| v.as_str()),
            Some("first!")
        );
    }

    #[test]
    fn finds_comment_by_base64_id_when_no_legacy_fbid() {
        let blocks = vec![comment_block()];
        let node = find_comment_node(&blocks, "222").unwrap();
        assert_eq!(
            node.pointer("/preferred_body/text")
                .and_then(|v| v.as_str()),
            Some("video reply")
        );
    }

    #[test]
    fn missing_comment_returns_none() {
        let blocks = vec![comment_block()];
        assert!(find_comment_node(&blocks, "333").is_none());
    }

    #[test]
    fn extracts_parsed_post_fields_from_comment_node() {
        let blocks = vec![comment_block()];
        let node = find_comment_node(&blocks, "222").unwrap();
        let post = parsed_post_from_comment(node, "reel/999?comment_id=222").unwrap();

        assert_eq!(post.author_name, "Bob (💬)");
        assert_eq!(post.text, "video reply");
        assert_eq!(post.date, 1750000100);
        assert_eq!(
            post.video_links,
            vec!["https://video.fbcdn.net/c.mp4".to_string()]
        );
        assert_eq!(
            post.thumbnail.as_deref(),
            Some("https://img.fbcdn.net/t.jpg")
        );
        assert!(post.image_links.is_empty());
        assert_eq!(post.url, "https://www.facebook.com/reel/999?comment_id=222");
    }

    #[test]
    fn text_only_comment_uses_feedback_url_and_reactions() {
        let blocks = vec![comment_block()];
        let node = find_comment_node(&blocks, "111").unwrap();
        let post = parsed_post_from_comment(node, "reel/999?comment_id=111").unwrap();

        assert_eq!(post.author_name, "Alice (💬)");
        assert_eq!(post.text, "first!");
        assert_eq!(post.likes, "7");
        assert_eq!(post.url, "https://www.facebook.com/x?comment_id=111");
        assert!(post.video_links.is_empty());
    }
}
