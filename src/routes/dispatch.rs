//! Pure path logic: which parser a Facebook path goes to, the rewrites
//! applied first, and the affinity scope key. No I/O.

use crate::url_clean;
use regex::Regex;
use std::sync::LazyLock;
use url::Url;

/// Apply the path rewrites, then pick the parser: image-in-comment and
/// comment permalinks first, then path shape. `None` means unsupported.
pub(super) fn route(path: &str) -> (String, Option<ParserKind>) {
    let mut path = path.to_owned();
    for rewrite in [
        group_multi_permalink_path,
        rewrite_slugged_photo_path,
        rewrite_videos_path,
        normalize_reel_path,
    ] {
        if let Some(rewritten) = rewrite(&path) {
            path = rewritten;
        }
    }
    let kind = if is_photocom(&path) {
        Some(ParserKind::Photocom)
    } else if crate::parsers::comment::comment_id_in(&path).is_some() {
        Some(ParserKind::Comment)
    } else {
        select_kind(&path)
    };
    (path, kind)
}

static RE_REEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/?reel/[0-9]+/?(?:\?.*)?$").unwrap());
static RE_REEL_TWO_SEGMENTS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/?reel/[0-9]+/[0-9]+/?$").unwrap());
// Only match bare `videos/<id>` (no Page prefix). Page-scoped video posts like
// `<page>/videos/<slug>/<id>` are real video viewer pages — not reels — and
// FB serves them with a watch-style JSON shape that the JsonPost root walker
// can't handle. Routed below to VideoWatchParser via [`RE_PAGE_VIDEO`].
static RE_VIDEOS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/?videos/(?:[^/]+/)?(\d+)").unwrap());
// `<page>/videos/<slug?>/<id>/` — FB Page video post viewer. Same JSON shape
// as /watch?v=<id>, so route to VideoWatchParser.
static RE_PAGE_VIDEO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/?[a-zA-Z0-9\-._]+/videos/(?:[^/]+/)?\d+").unwrap());
static RE_SLUGGED_PHOTO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/?[a-zA-Z0-9\-._]+/photos/[^/?]+/(\d+)/?(?:\?.*)?$").unwrap());
static RE_PHOTO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/*photo(\.php)*/*$").unwrap());
static RE_WATCH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/*watch").unwrap());
static RE_STORIES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/?stories/\d+/[A-Za-z0-9=_-]+").unwrap());

/// `share/`, `share/v/`, `share/p/`, `share/r/` short links, resolved by redirect.
pub(super) fn is_share_path(path: &str) -> bool {
    path.trim_start_matches('/').starts_with("share/")
}

fn is_facebook_url(path: &str) -> bool {
    let full = format!("https://www.facebook.com/{path}");
    let Ok(parsed) = Url::parse(&full) else {
        return false;
    };
    let p = parsed.path();
    let is_group = p.starts_with("/groups/");
    let is_permalink = p.starts_with("/permalink.php");
    let is_story = p.starts_with("/story.php");
    let mut prev = "";
    let mut is_post = false;
    for segment in p.trim_start_matches('/').split('/') {
        if segment == "posts"
            && !prev.is_empty()
            && prev
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        {
            is_post = true;
            break;
        }
        prev = segment;
    }
    let is_photo = p.starts_with("/photo");
    is_permalink || is_post || is_story || is_photo || is_group
}

fn path_only(s: &str) -> Option<String> {
    Url::parse(&format!(
        "https://www.facebook.com/{}",
        s.trim_start_matches('/')
    ))
    .ok()
    .map(|u| u.path().to_owned())
}

/// `videos/<slug?>/<id>[?query]` → `reel/<id>[?query]`. Query survives so
/// `comment_id` dispatch (ParserKind::Comment) still sees it.
fn rewrite_videos_path(working: &str) -> Option<String> {
    let caps = RE_VIDEOS.captures(working)?;
    let mut out = format!("reel/{}", &caps[1]);
    if let Some((_, query)) = working.split_once('?') {
        if !query.is_empty() {
            out.push('?');
            out.push_str(query);
        }
    }
    Some(out)
}

fn rewrite_slugged_photo_path(working: &str) -> Option<String> {
    let captures = RE_SLUGGED_PHOTO.captures(working)?;
    let mut out = format!("photo.php?fbid={}", &captures[1]);
    if let Some((_, query)) = working.split_once('?') {
        if !query.is_empty() {
            out.push('&');
            out.push_str(query);
        }
    }
    Some(out)
}

fn normalize_reel_path(working: &str) -> Option<String> {
    let (path, query) = working
        .split_once('?')
        .map_or((working, None), |(path, query)| (path, Some(query)));
    if !RE_REEL_TWO_SEGMENTS.is_match(path) {
        return None;
    }
    let id = path.rsplit('/').find(|segment| !segment.is_empty())?;
    let mut out = format!("reel/{id}");
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        out.push('?');
        out.push_str(query);
    }
    Some(out)
}

pub(super) fn select_kind(working: &str) -> Option<ParserKind> {
    if RE_STORIES.is_match(working) {
        Some(ParserKind::Stories)
    } else if RE_REEL.is_match(working) {
        Some(ParserKind::Reels)
    } else if path_only(working)
        .map(|p| RE_PHOTO.is_match(&p))
        .unwrap_or(false)
    {
        Some(ParserKind::SinglePhoto)
    } else if path_only(working)
        .map(|p| RE_WATCH.is_match(&p))
        .unwrap_or(false)
        || RE_PAGE_VIDEO.is_match(working)
    {
        Some(ParserKind::Watch)
    } else if is_facebook_url(working) {
        Some(ParserKind::JsonPost)
    } else {
        None
    }
}

/// `type=3` marks image-in-comment photo links (and, it turns out, plain
/// album photos; [`PhotocomParser`] falls back to a single-photo embed).
fn is_photocom(path: &str) -> bool {
    Url::parse(&url_clean::ensure_absolute(path))
        .map(|url| {
            url.query_pairs()
                .any(|(key, value)| key == "type" && value == "3")
        })
        .unwrap_or(false)
}

/// Drop comment_id/reply_comment_id so a failed comment lookup can re-dispatch
/// as the plain post/video it hangs off.
pub(super) fn strip_comment_id(path: &str) -> String {
    let Ok(url) = Url::parse(&url_clean::ensure_absolute(path)) else {
        return path.to_owned();
    };
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "comment_id" && k != "reply_comment_id")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut out = url.path().trim_start_matches('/').to_owned();
    if !kept.is_empty() {
        let mut tmp = Url::parse("https://www.facebook.com").unwrap();
        {
            let mut qp = tmp.query_pairs_mut();
            for (k, v) in &kept {
                qp.append_pair(k, v);
            }
        }
        if let Some(q) = tmp.query() {
            out.push('?');
            out.push_str(q);
        }
    }
    out
}

fn group_multi_permalink_path(s: &str) -> Option<String> {
    let parsed = Url::parse(&format!(
        "https://www.facebook.com/{}",
        s.trim_start_matches('/')
    ))
    .ok()?;
    let mut segments = parsed.path_segments()?;
    if segments.next()? != "groups" {
        return None;
    }
    let group = segments.next()?;
    if group.is_empty() {
        return None;
    }
    let post = parsed
        .query_pairs()
        .find(|(k, _)| k == "multi_permalinks")
        .map(|(_, v)| v.into_owned())?;
    if post.is_empty() || !post.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(format!("groups/{group}/posts/{post}/"))
}

/// Stable identifier for "this group" or "this user" used to pin a working
/// cookie account. Discord embeds go stale fast, so the second time a link
/// from the same group/profile lands we want to try the account that worked
/// last time before walking the fallback list.
///
/// Returns None for shapes that carry no group/profile (photo.php, reels,
/// watch) — those use the configured priority order. Visibility there is
/// per-post, so pinning every reel to the last winner only churned ordering.
pub(super) fn scope_key(path: &str) -> Option<String> {
    let p = path_only(path)?;
    let p = p.trim_start_matches('/');
    if let Some(rest) = p.strip_prefix("groups/") {
        let id = rest.split('/').next()?;
        if !id.is_empty() {
            return Some(format!("groups/{id}"));
        }
    }
    let mut parts = p.split('/');
    let first = parts.next()?;
    let second = parts.next()?;
    if !first.is_empty()
        && matches!(
            second,
            "posts" | "videos" | "photos" | "timeline" | "reels" | "media"
        )
    {
        return Some(format!("user/{first}"));
    }
    None
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ParserKind {
    JsonPost,
    SinglePhoto,
    Photocom,
    Reels,
    Watch,
    Stories,
    Comment,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn videos_rewrite_preserves_query() {
        assert_eq!(
            rewrite_videos_path("videos/123/?comment_id=456"),
            Some("reel/123?comment_id=456".to_string())
        );
        assert_eq!(
            rewrite_videos_path("videos/123/"),
            Some("reel/123".to_string())
        );
        assert_eq!(rewrite_videos_path("reel/123"), None);
    }

    #[test]
    fn two_segment_reel_paths_normalize_to_final_id_before_dispatch() {
        assert_eq!(
            normalize_reel_path("reel/1376968477584004/1013234327723021?mibextid=abc"),
            Some("reel/1013234327723021?mibextid=abc".to_string())
        );
        assert_eq!(normalize_reel_path("reel/1013234327723021"), None);
    }

    #[test]
    fn reel_dispatch_accepts_only_one_numeric_id_segment() {
        assert!(matches!(select_kind("reel/123"), Some(ParserKind::Reels)));
        assert!(matches!(select_kind("reel/123/"), Some(ParserKind::Reels)));
        assert!(matches!(
            select_kind("reel/123?x=1"),
            Some(ParserKind::Reels)
        ));
        assert!(select_kind("reel/1/2/3").is_none());
    }

    #[test]
    fn strip_comment_id_removes_only_comment_params() {
        assert_eq!(strip_comment_id("reel/999?comment_id=111"), "reel/999");
        assert_eq!(
            strip_comment_id("story.php?story_fbid=1&comment_id=2&id=3"),
            "story.php?story_fbid=1&id=3"
        );
        assert_eq!(
            strip_comment_id("reel/999?comment_id=1&reply_comment_id=2"),
            "reel/999"
        );
    }

    #[test]
    fn photocom_needs_exact_type_3() {
        assert!(super::is_photocom("photo.php?fbid=1&set=a.2&type=3"));
        assert!(!super::is_photocom("photo.php?fbid=1&type=13"));
        assert!(!super::is_photocom("photo.php?fbid=1"));
        assert_eq!(
            crate::url_clean::clean_path("photo.php?fbid=1&set=a.2&type=3&mibextid=x"),
            "photo.php?fbid=1&set=a.2&type=3"
        );
    }

    #[test]
    fn select_kind_routes_paths() {
        assert!(matches!(select_kind("reel/123"), Some(ParserKind::Reels)));
        assert!(matches!(select_kind("watch?v=1"), Some(ParserKind::Watch)));
        assert!(matches!(
            select_kind("groups/1/posts/2"),
            Some(ParserKind::JsonPost)
        ));
        assert!(select_kind("definitely-not-facebook").is_none());
    }

    #[test]
    fn group_path_extracts_group_id() {
        assert_eq!(
            scope_key("groups/12345/posts/678"),
            Some("groups/12345".into())
        );
        assert_eq!(scope_key("/groups/foo.bar"), Some("groups/foo.bar".into()));
    }

    #[test]
    fn user_post_path_extracts_username() {
        assert_eq!(scope_key("alice/posts/123"), Some("user/alice".into()));
        assert_eq!(scope_key("zuck/videos/abc/456"), Some("user/zuck".into()));
        assert_eq!(
            scope_key("page.name/photos/123"),
            Some("user/page.name".into())
        );
        assert_eq!(scope_key("u-name/reels/123"), Some("user/u-name".into()));
    }

    #[test]
    fn unscoped_paths_return_none() {
        assert_eq!(scope_key("photo.php?fbid=1&id=2"), None);
        assert_eq!(scope_key("permalink.php?story_fbid=1&id=2"), None);
        assert_eq!(scope_key("alice"), None);
    }

    #[test]
    fn query_string_does_not_affect_key() {
        assert_eq!(
            scope_key("groups/12345/posts/678?some=tracker"),
            Some("groups/12345".into())
        );
    }

    #[test]
    fn group_multi_permalink_rewrites_to_post_path() {
        assert_eq!(
            group_multi_permalink_path(
                "groups/364997627165697/?multi_permalinks=3055041888161244&x=1"
            ),
            Some("groups/364997627165697/posts/3055041888161244/".into())
        );
    }

    #[test]
    fn group_multi_permalink_ignores_invalid_shapes() {
        assert_eq!(group_multi_permalink_path("groups/12345/posts/678"), None);
        assert_eq!(group_multi_permalink_path("alice?multi_permalinks=1"), None);
        assert_eq!(
            group_multi_permalink_path("groups/12345/?multi_permalinks=../bad"),
            None
        );
    }

    #[test]
    fn facebook_url_dispatch_matches_supported_post_shapes() {
        assert!(is_facebook_url("groups/12345/posts/678"));
        assert!(is_facebook_url("alice/posts/678"));
        assert!(is_facebook_url("permalink.php?story_fbid=1&id=2"));
        assert!(is_facebook_url("story.php?story_fbid=1&id=2"));
        assert!(is_facebook_url("photo.php?fbid=1&id=2"));
        assert!(!is_facebook_url("share/p/abc"));
    }
}
