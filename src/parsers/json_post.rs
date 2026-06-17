use crate::error::{FacebedError, FacebedResult};
use crate::fetch::{get_json_block_texts, FetchedPage, JsonBlockText};
use crate::jq;
use crate::parsers::util::{interaction_counts, val_str_at, Story};
use crate::parsers::{banned_post, ParsedPost, Parser, ParserCtx};
use crate::url_clean::ensure_absolute;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

pub struct JsonPostParser;

#[async_trait::async_trait]
impl Parser for JsonPostParser {
    async fn process(&self, ctx: &ParserCtx, post_path: &str) -> FacebedResult<ParsedPost> {
        let post_id = extract_post_id(post_path);
        if should_try_partial_fetch(post_id.as_deref()) {
            let pid = post_id.clone().unwrap_or_default();
            let mut scanner = PostBlockScanner::default();
            let page = ctx
                .fetcher
                .fetch_until(post_path, true, |bytes| scanner.found_match(bytes, &pid))
                .await?;
            match parse_page(ctx, post_path, post_id.as_deref(), &page) {
                Ok(post) => return Ok(post),
                Err(e) if page.is_partial() => {
                    tracing::warn!(path = %post_path, error = %e, "partial post parse failed; retrying full fetch");
                }
                Err(e) => return Err(e),
            }
        }

        let page = ctx.fetcher.fetch(post_path, true).await?;
        parse_page(ctx, post_path, post_id.as_deref(), &page)
    }
}

fn parse_page(
    ctx: &ParserCtx,
    post_path: &str,
    post_id: Option<&str>,
    page: &FetchedPage,
) -> FacebedResult<ParsedPost> {
    let html = page.document();
    let blocks = get_json_block_texts(html, true);
    let post_json = get_post_json(&blocks, post_id).ok_or_else(|| {
        FacebedError::parse_with("cannot find post json", page.html.clone(), page.url.clone())
    })?;
    let root = get_root_node(&post_json).ok_or_else(|| {
        FacebedError::parse_with("Cannot process post", page.html.clone(), page.url.clone())
    })?;
    let (likes, cmts, shares) = interaction_counts(root)?;

    let post_date = root
        .pointer("/context_layout/story/comet_sections/metadata")
        .and_then(|m| jq::first(m, "creation_time"))
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(0);

    let story_json = root.pointer("/content/story").ok_or_else(|| {
        FacebedError::parse_with("missing content.story", page.html.clone(), page.url.clone())
    })?;
    let story = Story::from_json(story_json)?;

    let post_url = if story.url.is_empty() {
        ensure_absolute(post_path)
    } else {
        story.url.clone()
    };
    let post_content = story.get_text().trim().to_owned();
    let group_name = get_group_name(&blocks);
    let mut link_header = story.author_name.clone();
    if !group_name.is_empty() {
        link_header.push_str(" • ");
        link_header.push_str(&group_name);
    }

    if ctx.is_banned(&story.author_id) {
        return Ok(banned_post(&post_url));
    }

    let thumbnail = crate::parsers::util::thumbnail_in_node(story_json);

    Ok(ParsedPost {
        author_name: link_header,
        text: post_content,
        allow_discord_markdown: is_group_post_path(post_path),
        image_links: story.image_links,
        url: post_url,
        date: post_date,
        likes,
        comments: cmts,
        shares,
        video_links: story.video_links,
        thumbnail,
    })
}

/// Find the JSON block describing the requested post. When `post_id` is provided, only
/// blocks whose serialized payload mentions that ID are accepted — without this guard
/// FB feeds (group landing pages, ad-injected feeds) cause the parser to latch onto
/// whichever featured/suggested post happens to be the biggest block, returning a
/// completely unrelated embed (e.g. a Meta-for-Business ad).
fn get_post_json(blocks: &[JsonBlockText], post_id: Option<&str>) -> Option<Value> {
    // First pass: id-aware match.
    if let Some(pid) = post_id {
        for block in blocks {
            if !block.text.contains("i18n_reaction_count") || !block.text.contains(pid) {
                continue;
            }
            let Ok(bloc) = serde_json::from_str::<Value>(&block.text) else {
                continue;
            };
            if !jq::has(&bloc, &["i18n_reaction_count"]) {
                continue;
            }
            return Some(bloc);
        }
        // No block matched a numeric requested id — return None so the caller can
        // raise NoData/Parse instead of serving a wrong-post embed. Falling back
        // to any i18n_reaction_count block here is what produced the bug.
        //
        // pfbid URLs can be rewritten by FB to a different canonical pfbid inside
        // the JSON, so keep the older fallback for non-numeric ids.
        if pid.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
    }

    // No id available (very old paths) — fall back to first reaction block.
    for block in blocks {
        if !block.text.contains("i18n_reaction_count") {
            continue;
        }
        let Ok(bloc) = serde_json::from_str::<Value>(&block.text) else {
            continue;
        };
        if jq::has(&bloc, &["i18n_reaction_count"]) {
            return Some(bloc);
        }
    }
    None
}

fn should_try_partial_fetch(post_id: Option<&str>) -> bool {
    let Some(pid) = post_id else {
        return false;
    };
    pid.chars().all(|c| c.is_ascii_digit())
}

/// Incremental, forward-only scanner for the partial-fetch stop condition.
/// Returns `true` once a completed JSON script block has been seen whose body
/// contains both `i18n_reaction_count` and the requested `post_id`.
#[derive(Default)]
struct PostBlockScanner {
    /// Byte offset to resume the search for the next `<script` open tag.
    search_from: usize,
    /// Set while inside a script whose `</script>` has not arrived yet.
    open: Option<OpenScript>,
}

struct OpenScript {
    /// Index just past the `>` of the open tag.
    body_start: usize,
    /// Whether the open tag matched the Facebook JSON-block attributes.
    is_json_block: bool,
    /// Byte offset to resume the search for `</script>`.
    close_search_from: usize,
}

impl PostBlockScanner {
    fn found_match(&mut self, html: &[u8], post_id: &str) -> bool {
        const OPEN: &[u8] = b"<script";
        const CLOSE: &[u8] = b"</script>";

        loop {
            if let Some(open) = self.open.as_mut() {
                match find_bytes(&html[open.close_search_from..], CLOSE) {
                    Some(rel) => {
                        let close_start = open.close_search_from + rel;
                        if open.is_json_block {
                            let block = &html[open.body_start..close_start];
                            if contains_bytes(block, b"i18n_reaction_count")
                                && contains_bytes(block, post_id.as_bytes())
                            {
                                return true;
                            }
                        }
                        self.search_from = close_start + CLOSE.len();
                        self.open = None;
                    }
                    None => {
                        self.close_search_from_near_tail(html.len(), CLOSE.len());
                        return false;
                    }
                }
            } else {
                let Some(rel) = find_bytes(&html[self.search_from..], OPEN) else {
                    self.search_from = html.len().saturating_sub(OPEN.len() - 1);
                    return false;
                };
                let tag_start = self.search_from + rel;
                let Some(gt_rel) = find_bytes(&html[tag_start..], b">") else {
                    self.search_from = tag_start;
                    return false;
                };
                let body_start = tag_start + gt_rel + 1;
                let open_tag = &html[tag_start..body_start];
                let is_json_block = contains_bytes(open_tag, br#"type="application/json""#)
                    && contains_bytes(open_tag, b"data-content-len")
                    && contains_bytes(open_tag, b"data-sjs");
                self.open = Some(OpenScript {
                    body_start,
                    is_json_block,
                    close_search_from: body_start,
                });
            }
        }
    }

    fn close_search_from_near_tail(&mut self, html_len: usize, close_len: usize) {
        if let Some(open) = self.open.as_mut() {
            open.close_search_from = html_len.saturating_sub(close_len - 1).max(open.body_start);
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    find_bytes(haystack, needle).is_some()
}

static POST_ID_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?x)
        /posts/(?:[^/?]+/)?([A-Za-z0-9]+)
        | /permalink/([A-Za-z0-9]+)
        | [?&]story_fbid=([A-Za-z0-9]+)
        | [?&]multi_permalinks=([A-Za-z0-9]+)
        | /videos/(?:[^/?]+/)?([A-Za-z0-9]+)
        | /reel/([A-Za-z0-9]+)
        ",
    )
    .unwrap()
});

fn extract_post_id(post_path: &str) -> Option<String> {
    POST_ID_RE
        .captures(post_path)?
        .iter()
        .skip(1)
        .find_map(|m| m.map(|x| x.as_str().to_owned()))
}

fn is_group_post_path(post_path: &str) -> bool {
    post_path.trim_start_matches('/').starts_with("groups/")
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

fn get_group_name(blocks: &[JsonBlockText]) -> String {
    for block in blocks {
        if !block.text.contains("group_member_profiles")
            || !block.text.contains("formatted_count_text")
        {
            continue;
        }
        let Ok(bloc) = serde_json::from_str::<Value>(&block.text) else {
            continue;
        };
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

#[cfg(test)]
mod tests {
    use super::{extract_post_id, is_group_post_path, should_try_partial_fetch, PostBlockScanner};

    #[test]
    fn extracts_numeric_post_id() {
        assert_eq!(
            extract_post_id("groups/ThinkPadViet/posts/2532721970496550/"),
            Some("2532721970496550".into())
        );
    }

    #[test]
    fn detects_group_post_path() {
        assert!(is_group_post_path(
            "groups/sportsbook6vn/posts/1372570601398684/"
        ));
        assert!(is_group_post_path(
            "/groups/sportsbook6vn/posts/1372570601398684/"
        ));
        assert!(!is_group_post_path("some.page/posts/1372570601398684/"));
    }

    #[test]
    fn extracts_slugged_page_post_id() {
        assert_eq!(
            extract_post_id("thongtinchinhphu/posts/-some-long-slug-/1458346319663477/"),
            Some("1458346319663477".into())
        );
    }

    #[test]
    fn extracts_pfbid_post_id() {
        assert_eq!(
            extract_post_id("clark.leonard1406/posts/pfbid02XkxJdpbABcHdqmcA1Je8rNNYXVpt5QELruUEnrii5iU1fVGmt8qZECEcoHsbSDiBl"),
            Some("pfbid02XkxJdpbABcHdqmcA1Je8rNNYXVpt5QELruUEnrii5iU1fVGmt8qZECEcoHsbSDiBl".into())
        );
    }

    #[test]
    fn extracts_query_post_ids() {
        assert_eq!(
            extract_post_id("groups/1030569618932119/?multi_permalinks=1349761413679603&x=1"),
            Some("1349761413679603".into())
        );
        assert_eq!(
            extract_post_id("story.php?story_fbid=pfbid02abc&id=123"),
            Some("pfbid02abc".into())
        );
    }

    #[test]
    fn partial_fetch_only_for_numeric_posts() {
        assert!(should_try_partial_fetch(Some("123")));
        assert!(!should_try_partial_fetch(Some("pfbid02abc")));
        assert!(!should_try_partial_fetch(None));
    }

    #[test]
    fn matching_post_block_requires_completed_script() {
        let open = r#"<script type="application/json" data-content-len="42" data-sjs>"#;
        let body = r#"{"i18n_reaction_count":"1K","id":"123"}"#;

        let mut scanner = PostBlockScanner::default();
        assert!(!scanner.found_match(format!("{open}{body}").as_bytes(), "123"));

        let mut scanner = PostBlockScanner::default();
        assert!(scanner.found_match(
            format!("{open}{body}</script><div>later</div>").as_bytes(),
            "123"
        ));

        let mut scanner = PostBlockScanner::default();
        assert!(!scanner.found_match(format!("{open}{body}</script>").as_bytes(), "456"));
    }

    #[test]
    fn scanner_matches_across_chunk_boundaries() {
        let open = r#"<script type="application/json" data-content-len="42" data-sjs>"#;
        let body = r#"{"i18n_reaction_count":"1K","id":"123"}"#;
        let full = format!("{open}{body}</script>");
        let bytes = full.as_bytes();

        let mut scanner = PostBlockScanner::default();
        let mut fired_at = None;
        for end in 1..=bytes.len() {
            if scanner.found_match(&bytes[..end], "123") {
                fired_at = Some(end);
                break;
            }
        }

        assert_eq!(fired_at, Some(bytes.len()));
    }
}
