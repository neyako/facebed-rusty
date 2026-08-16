use crate::error::{FacebedError, FacebedResult};
use crate::fetch::{get_json_block_texts, FetchedPage, JsonBlockText};
use crate::jq;
use crate::parsers::util::{interaction_counts_with_reaction_ids, Story};
use crate::parsers::{banned_post, ParsedPost, Parser, ParserCtx};
use crate::url_clean::{self, ensure_absolute};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use url::Url;

pub struct JsonPostParser;

struct ParsedPostDraft {
    post: ParsedPost,
    unresolved_author_id: Option<String>,
}

#[async_trait::async_trait]
impl Parser for JsonPostParser {
    async fn process(&self, ctx: &ParserCtx, post_path: &str) -> FacebedResult<ParsedPost> {
        let post_id = extract_post_id(post_path);
        if should_try_partial_fetch(post_id.as_deref()) {
            let pid = post_id.clone().unwrap_or_default();
            let mut scanner = PostBlockScanner::default();
            let (is_partial, parsed) = {
                let page = ctx
                    .fetcher
                    .fetch_until(post_path, true, |bytes| scanner.found_match(bytes, &pid))
                    .await?;
                (
                    page.is_partial(),
                    parse_page(ctx, post_path, post_id.as_deref(), &page),
                )
            };
            match parsed {
                Ok(draft) => return Ok(resolve_author_handle(ctx, draft).await),
                Err(e) if is_partial => {
                    tracing::warn!(path = %post_path, error = %e, "partial post parse failed; retrying full fetch");
                }
                Err(e) => return Err(e),
            }
        }

        let parsed = {
            let page = ctx.fetcher.fetch(post_path, true).await?;
            parse_page(ctx, post_path, post_id.as_deref(), &page)
        };
        Ok(resolve_author_handle(ctx, parsed?).await)
    }
}

async fn resolve_author_handle(ctx: &ParserCtx, mut draft: ParsedPostDraft) -> ParsedPost {
    if let Some(author_id) = draft.unresolved_author_id {
        if let Some(handle) = ctx.fetcher.resolve_profile_handle(&author_id).await {
            draft.post.author_handle = Some(handle);
        }
    }
    draft.post
}

pub(crate) fn parse_fetched_post(
    ctx: &ParserCtx,
    post_path: &str,
    page: &FetchedPage,
) -> FacebedResult<ParsedPost> {
    let post_id = extract_post_id(post_path);
    parse_page(ctx, post_path, post_id.as_deref(), page).map(|draft| draft.post)
}

fn parse_page(
    ctx: &ParserCtx,
    post_path: &str,
    post_id: Option<&str>,
    page: &FetchedPage,
) -> FacebedResult<ParsedPostDraft> {
    let html = page.document();
    let blocks = get_json_block_texts(html, true);
    let canonical_post_id = canonical_page_post_id(html);
    let post_json = get_post_json_for_page(&blocks, post_id, canonical_post_id.as_deref())
        .ok_or_else(|| {
            FacebedError::parse_with("cannot find post json", page.html.clone(), page.url.clone())
        })?;
    let root = get_root_node(&post_json).ok_or_else(|| {
        FacebedError::parse_with("Cannot process post", page.html.clone(), page.url.clone())
    })?;
    let story_json = root.pointer("/content/story").ok_or_else(|| {
        FacebedError::parse_with("missing content.story", page.html.clone(), page.url.clone())
    })?;
    let interaction_ids =
        interaction_identity_candidates(post_id, canonical_post_id.as_deref(), story_json);
    let interaction_refs = interaction_ids
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let (likes, cmts, shares, top_reaction_ids) =
        interaction_counts_with_reaction_ids(root, &interaction_refs)?;

    let post_date = root
        .pointer("/context_layout/story/comet_sections/metadata")
        .and_then(|m| jq::first(m, "creation_time"))
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(0);

    let story = Story::from_json(story_json)?;

    let post_url = if story.url.is_empty() {
        ensure_absolute(post_path)
    } else {
        story.url.clone()
    };
    let post_content = story.get_text().trim().to_owned();
    let group_handle = group_handle_from_post_path(post_path).map(str::to_owned);
    let embedded_handle = story.author_handle.clone();
    let unresolved_author_id = embedded_handle
        .is_none()
        .then(|| story.author_id.clone())
        .filter(|author_id| !author_id.is_empty());
    let author_handle = embedded_handle.or(group_handle);

    if ctx.is_banned(&story.author_id) {
        return Ok(ParsedPostDraft {
            post: banned_post(&post_url),
            unresolved_author_id: None,
        });
    }

    let thumbnail = crate::parsers::util::thumbnail_in_node(story_json);
    let context = story.context();

    Ok(ParsedPostDraft {
        post: ParsedPost {
            author_name: story.author_name,
            author_id: (!story.author_id.is_empty()).then_some(story.author_id),
            author_handle,
            author_avatar_url: story.author_avatar_url,
            context,
            text: post_content,
            allow_discord_markdown: is_group_post_path(post_path),
            image_links: story.image_links,
            url: post_url,
            date: post_date,
            likes,
            top_reaction_ids,
            comments: cmts,
            shares,
            video_links: story.video_links,
            thumbnail,
        },
        unresolved_author_id,
    })
}

fn interaction_identity_candidates(
    post_id: Option<&str>,
    canonical_post_id: Option<&str>,
    story_json: &Value,
) -> Vec<String> {
    let mut ids = Vec::new();
    for id in [post_id, canonical_post_id]
        .into_iter()
        .flatten()
        .filter(|id| !id.is_empty())
    {
        if !ids.iter().any(|candidate| candidate == id) {
            ids.push(id.to_owned());
        }
    }
    if let Some(id) = story_json
        .get("post_id")
        .map(crate::parsers::util::val_str)
        .filter(|id| !id.is_empty())
    {
        if !ids.iter().any(|candidate| candidate == &id) {
            ids.push(id);
        }
    }
    ids
}

/// Select the requested story using its URL ID or Facebook's page-canonical post ID.
#[cfg(test)]
fn get_post_json(blocks: &[JsonBlockText], post_id: Option<&str>) -> Option<Value> {
    get_post_json_for_page(blocks, post_id, None)
}

fn get_post_json_for_page(
    blocks: &[JsonBlockText],
    post_id: Option<&str>,
    canonical_post_id: Option<&str>,
) -> Option<Value> {
    // First pass: id-aware match.
    if let Some(pid) = post_id {
        for block in blocks {
            let contains_candidate_id = block.text.contains(pid)
                || canonical_post_id.is_some_and(|id| block.text.contains(id));
            if !contains_candidate_id {
                continue;
            }
            let Ok(bloc) = serde_json::from_str::<Value>(&block.text) else {
                continue;
            };
            let Some(story) = get_root_node(&bloc).and_then(|root| root.pointer("/content/story"))
            else {
                continue;
            };
            if story_matches_post_id(story, pid)
                || canonical_post_id.is_some_and(|id| story_matches_post_id(story, id))
            {
                return Some(bloc);
            }
        }
        return None;
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

fn story_matches_post_id(story: &Value, post_id: &str) -> bool {
    story
        .get("post_id")
        .is_some_and(|value| value_matches_post_id(value, post_id))
        || story
            .get("wwwURL")
            .and_then(Value::as_str)
            .and_then(canonical_story_post_id)
            .as_deref()
            == Some(post_id)
}

fn value_matches_post_id(value: &Value, post_id: &str) -> bool {
    match value {
        Value::String(value) => value == post_id,
        Value::Number(value) => value.to_string() == post_id,
        _ => false,
    }
}

static PAGE_CANONICAL_LINK_SEL: Lazy<scraper::Selector> =
    Lazy::new(|| scraper::Selector::parse(r#"link[rel="canonical"]"#).unwrap());
static PAGE_OG_URL_SEL: Lazy<scraper::Selector> =
    Lazy::new(|| scraper::Selector::parse(r#"meta[property="og:url"]"#).unwrap());

fn canonical_page_post_id(html: &scraper::Html) -> Option<String> {
    [
        (&*PAGE_CANONICAL_LINK_SEL, "href"),
        (&*PAGE_OG_URL_SEL, "content"),
    ]
    .into_iter()
    .find_map(|(selector, attribute)| {
        html.select(selector)
            .next()
            .and_then(|element| element.value().attr(attribute))
            .and_then(canonical_story_post_id)
    })
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

fn canonical_story_post_id(story_url: &str) -> Option<String> {
    if story_url.trim() != story_url || story_url.contains('\\') {
        return None;
    }
    let parsed = Url::parse(story_url).ok()?;
    let host = parsed.host_str()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !url_clean::is_facebook_page_host(host)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
    {
        return None;
    }

    let identity_queries = parsed
        .query_pairs()
        .filter(|(key, _)| matches!(key.as_ref(), "story_fbid" | "multi_permalinks"))
        .collect::<Vec<_>>();

    let path = parsed.path().strip_prefix('/')?;
    let path = path.strip_suffix('/').unwrap_or(path);
    if path.is_empty() {
        return None;
    }
    let segments = path.split('/').collect::<Vec<_>>();
    if segments.iter().any(|segment| segment.is_empty()) {
        return None;
    }
    let path_id = match segments.as_slice() {
        ["groups", _, "posts", id] | ["groups", _, "permalink", id] => Some(*id),
        [owner, "posts", id] if *owner != "groups" => Some(*id),
        [owner, "posts", _, id] if *owner != "groups" => Some(*id),
        ["permalink", id] => Some(*id),
        _ => None,
    };
    if let Some(id) = path_id {
        if !identity_queries.is_empty() {
            return None;
        }
        return valid_post_id(id).then(|| id.to_owned());
    }

    let query_key = if matches!(segments.as_slice(), ["story.php"] | ["permalink.php"]) {
        "story_fbid"
    } else if matches!(segments.as_slice(), ["groups", _]) {
        "multi_permalinks"
    } else {
        return None;
    };

    let [(key, id)] = identity_queries.as_slice() else {
        return None;
    };
    if key.as_ref() != query_key || !valid_post_id(id) {
        return None;
    }
    Some(id.to_string())
}

fn valid_post_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric())
}

fn is_group_post_path(post_path: &str) -> bool {
    post_path.trim_start_matches('/').starts_with("groups/")
}

fn group_handle_from_post_path(post_path: &str) -> Option<&str> {
    let mut segments = post_path
        .split('?')
        .next()?
        .trim_start_matches('/')
        .split('/');
    match (segments.next(), segments.next()) {
        (Some("groups"), Some(handle)) if !handle.is_empty() => Some(handle),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::{
        canonical_page_post_id, extract_post_id, get_post_json, get_post_json_for_page,
        get_root_node, group_handle_from_post_path, interaction_identity_candidates,
        is_group_post_path, should_try_partial_fetch, PostBlockScanner,
    };
    use crate::fetch::JsonBlockText;
    use scraper::Html;
    use serde_json::json;

    fn post_block(story_url: &str, marker: &str) -> JsonBlockText {
        JsonBlockText {
            text: json!({
                "i18n_reaction_count": "1",
                "data": {
                    "comet_ufi_summary_and_actions_renderer": {},
                    "content": { "story": {
                        "wwwURL": story_url,
                        "actors": [{"id": marker, "name": marker}],
                        "message": {"text": marker}
                    }}
                }
            })
            .to_string(),
        }
    }

    #[test]
    fn pfbid_canonical_rewrite_without_exact_id_fails_closed() {
        let mut unrelated = post_block(
            "https://www.facebook.com/quata.pham/posts/pfbidUNRELATED",
            "unrelated",
        );
        let mut unrelated_json: serde_json::Value = serde_json::from_str(&unrelated.text).unwrap();
        unrelated_json["data"]["content"]["story"]["request_id"] = json!("pfbidREQUESTED");
        unrelated.text = unrelated_json.to_string();
        let blocks = vec![
            unrelated,
            post_block(
                "https://www.facebook.com/dantech0xff/posts/pfbidCANONICAL",
                "target",
            ),
        ];

        assert!(get_post_json(&blocks, Some("pfbidREQUESTED")).is_none());
    }

    #[test]
    fn pfbid_rewrite_accepts_story_proven_by_page_canonical_post_id() {
        // Given
        let mut target = post_block(
            "https://www.facebook.com/dantech0xff/posts/pfbidCANONICAL",
            "target",
        );
        let mut target_json: serde_json::Value = serde_json::from_str(&target.text).unwrap();
        target_json["data"]["content"]["story"]["post_id"] = json!("28131981629721302");
        target.text = target_json.to_string();
        let blocks = vec![
            post_block(
                "https://www.facebook.com/quata.pham/posts/pfbidUNRELATED",
                "unrelated",
            ),
            target,
        ];

        // When
        let selected =
            get_post_json_for_page(&blocks, Some("pfbidREQUESTED"), Some("28131981629721302"))
                .unwrap();

        // Then
        assert_eq!(
            get_root_node(&selected)
                .and_then(|root| root.pointer("/content/story/post_id"))
                .and_then(|id| id.as_str()),
            Some("28131981629721302")
        );
    }

    #[test]
    fn page_canonical_post_id_extracts_slugged_numeric_permalink() {
        // Given
        let html = Html::parse_document(
            r#"<meta property="og:url" content="https://www.facebook.com/volecong/posts/summer-trip/28131981629721302/">"#,
        );

        // When / Then
        assert_eq!(
            canonical_page_post_id(&html).as_deref(),
            Some("28131981629721302")
        );
    }

    #[test]
    fn pfbid_rewrite_rejects_ambiguous_stories_from_requested_owner() {
        let blocks = vec![
            post_block(
                "https://www.facebook.com/dantech0xff/posts/pfbidFIRST",
                "first",
            ),
            post_block(
                "https://www.facebook.com/dantech0xff/posts/pfbidSECOND",
                "second",
            ),
        ];

        assert!(get_post_json(&blocks, Some("pfbidREQUESTED")).is_none());
    }

    #[test]
    fn pfbid_exact_match_ignores_requested_id_in_unrelated_story_query() {
        let blocks = vec![
            post_block(
                "https://www.facebook.com/quata.pham/posts/pfbidUNRELATED?next=pfbidREQUESTED",
                "unrelated",
            ),
            post_block(
                "https://www.facebook.com/dantech0xff/posts/pfbidREQUESTED",
                "target",
            ),
        ];

        let selected = get_post_json(&blocks, Some("pfbidREQUESTED")).unwrap();

        assert_eq!(
            get_root_node(&selected)
                .and_then(|root| root.pointer("/content/story/wwwURL"))
                .and_then(|url| url.as_str()),
            Some("https://www.facebook.com/dantech0xff/posts/pfbidREQUESTED")
        );
    }

    #[test]
    fn exact_id_match_accepts_structured_reaction_count_without_i18n_label() {
        // Given
        let mut target = post_block(
            "https://www.facebook.com/lazzyrabbii/posts/pfbidREQUESTED",
            "target",
        );
        let mut target_json: serde_json::Value = serde_json::from_str(&target.text).unwrap();
        target_json
            .as_object_mut()
            .unwrap()
            .remove("i18n_reaction_count");
        target_json["data"]["comet_ufi_summary_and_actions_renderer"]["feedback"] = json!({"adaptive_ufi_action_renderers": [{
            "feedback": {"reaction_count": {"count": 0}}
        }]});
        target.text = target_json.to_string();

        // When
        let selected = get_post_json(&[target], Some("pfbidREQUESTED"));

        // Then
        assert!(
            selected.is_some(),
            "exact story identity should not require an i18n reaction label"
        );
    }

    #[test]
    fn canonical_rewrite_carries_all_focal_feedback_ids() {
        let story = json!({"post_id": "28131981629721302"});
        let ids = interaction_identity_candidates(
            Some("pfbidREQUESTED"),
            Some("28131981629721302"),
            &story,
        );

        assert_eq!(ids, vec!["pfbidREQUESTED", "28131981629721302"]);
    }

    #[test]
    fn numeric_id_match_ignores_requested_id_in_unrelated_story_data() {
        let mut unrelated =
            post_block("https://www.facebook.com/quata.pham/posts/999", "unrelated");
        let mut unrelated_json: serde_json::Value = serde_json::from_str(&unrelated.text).unwrap();
        unrelated_json["data"]["content"]["story"]["request_id"] = json!("123");
        unrelated.text = unrelated_json.to_string();
        let blocks = vec![
            unrelated,
            post_block("https://www.facebook.com/dantech0xff/posts/123", "target"),
        ];

        let selected = get_post_json(&blocks, Some("123")).unwrap();

        assert_eq!(
            get_root_node(&selected)
                .and_then(|root| root.pointer("/content/story/wwwURL"))
                .and_then(|url| url.as_str()),
            Some("https://www.facebook.com/dantech0xff/posts/123")
        );
    }

    #[test]
    fn pfbid_rewrite_rejects_matching_owner_from_non_facebook_host() {
        let blocks = vec![post_block(
            "https://example.com/dantech0xff/posts/pfbidCANONICAL",
            "external",
        )];

        assert!(get_post_json(&blocks, Some("pfbidREQUESTED")).is_none());
    }

    #[test]
    fn pfbid_exact_match_rejects_non_facebook_story_url() {
        let blocks = vec![post_block(
            "https://example.com/dantech0xff/posts/pfbidREQUESTED",
            "external",
        )];

        assert!(get_post_json(&blocks, Some("pfbidREQUESTED")).is_none());
    }

    #[test]
    fn pfbid_exact_match_ignores_requested_path_inside_unrelated_query() {
        let blocks = vec![post_block(
            "https://www.facebook.com/somewhere?next=/posts/pfbidREQUESTED",
            "unrelated",
        )];

        assert!(get_post_json(&blocks, Some("pfbidREQUESTED")).is_none());
    }

    #[test]
    fn pfbid_exact_match_rejects_story_fbid_on_unrelated_path() {
        let blocks = vec![post_block(
            "https://www.facebook.com/somewhere?story_fbid=pfbidREQUESTED",
            "unrelated",
        )];

        assert!(get_post_json(&blocks, Some("pfbidREQUESTED")).is_none());
    }

    #[test]
    fn pfbid_exact_match_rejects_protocol_relative_external_url() {
        let blocks = vec![post_block("//example.com/posts/pfbidREQUESTED", "external")];

        assert!(get_post_json(&blocks, Some("pfbidREQUESTED")).is_none());
    }

    #[test]
    fn pfbid_exact_match_accepts_story_php_identity_query() {
        let blocks = vec![post_block(
            "https://www.facebook.com/story.php?story_fbid=pfbidREQUESTED&id=123",
            "target",
        )];

        assert!(get_post_json(&blocks, Some("pfbidREQUESTED")).is_some());
    }

    #[test]
    fn numeric_exact_match_accepts_group_multi_permalink_identity_query() {
        let blocks = vec![post_block(
            "https://www.facebook.com/groups/123/?multi_permalinks=456",
            "target",
        )];

        assert!(get_post_json(&blocks, Some("456")).is_some());
    }

    #[test]
    fn pfbid_exact_match_rejects_ambiguous_identity_path() {
        let blocks = vec![post_block(
            "https://www.facebook.com/owner/posts/slug/pfbidREQUESTED/permalink/pfbidOTHER",
            "ambiguous",
        )];

        assert!(get_post_json(&blocks, Some("pfbidREQUESTED")).is_none());
    }

    #[test]
    fn pfbid_exact_match_rejects_empty_path_segments() {
        for story_url in [
            "https://www.facebook.com/owner//posts/pfbidREQUESTED",
            "https://www.facebook.com//owner/posts/pfbidREQUESTED",
        ] {
            let blocks = vec![post_block(story_url, "malformed")];

            assert!(get_post_json(&blocks, Some("pfbidREQUESTED")).is_none());
        }
    }

    #[test]
    fn pfbid_exact_match_rejects_conflicting_identity_query() {
        let blocks = vec![post_block(
            "https://www.facebook.com/owner/posts/pfbidREQUESTED?story_fbid=pfbidOTHER",
            "ambiguous",
        )];

        assert!(get_post_json(&blocks, Some("pfbidREQUESTED")).is_none());
    }

    #[test]
    fn pfbid_exact_match_rejects_noncanonical_url_syntax() {
        for story_url in [
            "https://www.facebook.com:444/owner/posts/pfbidREQUESTED",
            "https://www.facebook.com/owner\\posts\\pfbidREQUESTED",
        ] {
            let blocks = vec![post_block(story_url, "noncanonical")];

            assert!(get_post_json(&blocks, Some("pfbidREQUESTED")).is_none());
        }
    }

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
    fn group_handle_uses_group_vanity_segment() {
        // Given / When / Then
        assert_eq!(
            group_handle_from_post_path("groups/cuongsac/permalink/1440980791332233/"),
            Some("cuongsac")
        );
        assert_eq!(group_handle_from_post_path("some.page/posts/123"), None);
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
