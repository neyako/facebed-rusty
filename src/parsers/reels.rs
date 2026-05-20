use crate::error::{FacebedError, FacebedResult};
use crate::fetch::get_json_blocks;
use crate::jq;
use crate::parsers::util::{human_format, val_str_at, video_link_in_node};
use crate::parsers::{banned_post, ParsedPost, Parser, ParserCtx};
use scraper::Html;
use serde_json::Value;

pub struct ReelsParser;

#[async_trait::async_trait]
impl Parser for ReelsParser {
    async fn process(&self, ctx: &ParserCtx, post_path: &str) -> FacebedResult<ParsedPost> {
        let page = ctx.fetcher.fetch(post_path, true).await?;
        let html = page.parse();
        let blocks = get_json_blocks(&html, true);

        let content_node = find_content_node(&blocks).ok_or_else(|| {
            FacebedError::parse_with("Invalid reels link (cn)", page.html.clone(), page.url.clone())
        })?;

        let video_link = find_video_link(&blocks, &content_node).ok_or_else(|| {
            FacebedError::parse_with("Invalid reels link (vn)", page.html.clone(), page.url.clone())
        })?;

        let video_id = val_str_at(&content_node, "id").unwrap_or("").to_owned();

        let owner = find_owner_with_name(&blocks).ok_or_else(|| {
            FacebedError::parse_with("Invalid reels link (own)", page.html.clone(), page.url.clone())
        })?;
        let typename = val_str_at(&owner, "__typename").unwrap_or("");
        let is_ig = typename.starts_with("InstagramUser");
        let op_name = if is_ig {
            format!("📷 @{}", val_str_at(&owner, "username").unwrap_or(""))
        } else {
            val_str_at(&owner, "name").unwrap_or("").to_owned()
        };
        let owner_id = val_str_at(&owner, "id").unwrap_or("").to_owned();

        let post_url = find_shareable_url(&blocks)
            .unwrap_or_else(|| crate::url_clean::ensure_absolute(post_path));

        let date = find_creation_time(&blocks).unwrap_or(0);
        let post_text = find_message_text(&blocks);

        let (likes, cmts, shares) = get_reaction_counts(&blocks, is_ig, &video_id)
            .unwrap_or(("null".into(), "null".into(), "null".into()));

        if ctx.is_banned(&owner_id) {
            return Ok(banned_post(&post_url));
        }

        Ok(ParsedPost {
            author_name: op_name,
            text: post_text,
            image_links: Vec::new(),
            url: post_url,
            date,
            likes,
            comments: cmts,
            shares,
            video_links: vec![video_link],
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

/// Search every block for an `owner` object that has a `name` field
/// (i.e. the "rich" owner, not just `{id, __typename}`).
fn find_owner_with_name(blocks: &[Value]) -> Option<Value> {
    for bloc in blocks {
        // prefer short_form_video_context.video_owner (always has name)
        if let Some(o) = bloc.pointer_path_first(&["short_form_video_context", "video_owner"]) {
            return Some(o.clone());
        }
    }
    for bloc in blocks {
        for o in jq::all(bloc, "video_owner") {
            if o.get("name").and_then(|v| v.as_str()).is_some() {
                return Some(o.clone());
            }
        }
        for o in jq::all(bloc, "owner") {
            if o.get("name").and_then(|v| v.as_str()).is_some() {
                return Some(o.clone());
            }
        }
    }
    None
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

    let feedbacks = jq::all(bloc, "feedback");
    let first_fb = feedbacks.first().copied()?;
    let last_fb = feedbacks.last().copied()?;
    let (first_fb, last_fb) = if first_fb.to_string().contains("cross_universe_feedback_info") {
        (last_fb, first_fb)
    } else {
        (first_fb, last_fb)
    };

    let ig_cmts = last_fb
        .pointer("/cross_universe_feedback_info/ig_comment_count")
        .cloned()
        .unwrap_or(Value::Null);
    let likes = first_fb
        .pointer("/unified_reactors/count")
        .cloned()
        .unwrap_or(Value::Null);
    let cmts = if is_ig {
        ig_cmts
    } else {
        last_fb
            .get("total_comment_count")
            .cloned()
            .unwrap_or(Value::Null)
    };
    let shares = last_fb
        .get("share_count_reduced")
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
