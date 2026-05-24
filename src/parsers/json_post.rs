use crate::error::{FacebedError, FacebedResult};
use crate::fetch::get_json_blocks;
use crate::jq;
use crate::parsers::util::{interaction_counts, val_str_at, Story};
use crate::parsers::{banned_post, ParsedPost, Parser, ParserCtx};
use crate::url_clean::ensure_absolute;
use once_cell::sync::Lazy;
use regex::Regex;
use scraper::Html;
use serde_json::Value;

pub struct JsonPostParser;

#[async_trait::async_trait]
impl Parser for JsonPostParser {
    async fn process(&self, ctx: &ParserCtx, post_path: &str) -> FacebedResult<ParsedPost> {
        let page = ctx.fetcher.fetch(post_path, true).await?;
        let html = page.parse();
        let post_id = extract_post_id(post_path);
        let post_json = get_post_json(&html, post_id.as_deref()).ok_or_else(|| {
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

        let thumbnail = crate::parsers::util::thumbnail_in_node(story_json);

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
            thumbnail,
        })
    }
}

/// Find the JSON block describing the requested post. When `post_id` is provided, only
/// blocks whose serialized payload mentions that ID are accepted — without this guard
/// FB feeds (group landing pages, ad-injected feeds) cause the parser to latch onto
/// whichever featured/suggested post happens to be the biggest block, returning a
/// completely unrelated embed (e.g. a Meta-for-Business ad).
fn get_post_json(html: &Html, post_id: Option<&str>) -> Option<Value> {
    // First pass: id-aware match.
    if let Some(pid) = post_id {
        for bloc in get_json_blocks(html, true) {
            if !jq::has(&bloc, &["i18n_reaction_count"]) {
                continue;
            }
            let s = serde_json::to_string(&bloc).unwrap_or_default();
            if s.contains(pid) {
                return Some(bloc);
            }
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
    for bloc in get_json_blocks(html, true) {
        if jq::has(&bloc, &["i18n_reaction_count"]) {
            return Some(bloc);
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::extract_post_id;

    #[test]
    fn extracts_numeric_post_id() {
        assert_eq!(
            extract_post_id("groups/ThinkPadViet/posts/2532721970496550/"),
            Some("2532721970496550".into())
        );
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
}
