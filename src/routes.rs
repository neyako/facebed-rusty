use crate::config::Config;
use crate::cookies::NOTIFY_FAILURE_THRESHOLD;
use crate::crawler;
use crate::embed::{
    format_error_embed, format_full_post_embed, format_oversized_video_embed, format_redirect_page,
    format_reel_post_embed, format_timeout_embed,
};
use crate::error::FacebedError;
use crate::fetch::{resolve_share_link, Fetcher, ACCOUNT_OVERRIDE};
use crate::notifier::Notifier;
use crate::parsers::{
    json_post::JsonPostParser, photocom::PhotocomParser, reels::ReelsParser,
    single_photo::SinglePhotoParser, stories::StoriesParser, video_watch::VideoWatchParser,
    ParsedPost, Parser, ParserCtx,
};
use crate::url_clean;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use once_cell::sync::Lazy;
use regex::Regex;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use url::Url;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub ctx: Arc<ParserCtx>,
    pub notifier: Notifier,
    pub fetcher: Arc<Fetcher>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/favicon.ico", get(favicon))
        .route("/banner.png", get(banner))
        .route("/*path", get(catch_all))
        .with_state(state)
}

async fn root() -> impl IntoResponse {
    match tokio::fs::read_to_string("assets/index.html").await {
        Ok(s) => {
            let body = s.replace("{|CREDIT|}", crate::embed::credit());
            html_response(body)
        }
        Err(_) => (StatusCode::NOT_FOUND, "").into_response(),
    }
}

async fn favicon() -> impl IntoResponse {
    static_asset("assets/favicon.ico", "image/x-icon").await
}

async fn banner() -> impl IntoResponse {
    static_asset("assets/banner.png", "image/png").await
}

async fn static_asset(path: &str, ct: &'static str) -> Response {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static(ct),
            );
            (StatusCode::OK, headers, bytes).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "").into_response(),
    }
}

fn html_response(body: String) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    (StatusCode::OK, headers, body).into_response()
}

fn no_store_html_response(body: String) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    headers.insert(
        axum::http::header::PRAGMA,
        HeaderValue::from_static("no-cache"),
    );
    (StatusCode::OK, headers, body).into_response()
}

static RE_REEL: Lazy<Regex> = Lazy::new(|| Regex::new(r"^/?reel/[0-9]+").unwrap());
// Only match bare `videos/<id>` (no Page prefix). Page-scoped video posts like
// `<page>/videos/<slug>/<id>` are real video viewer pages — not reels — and
// FB serves them with a watch-style JSON shape that the JsonPost root walker
// can't handle. Routed below to VideoWatchParser via [`RE_PAGE_VIDEO`].
static RE_VIDEOS: Lazy<Regex> = Lazy::new(|| Regex::new(r"^/?videos/(?:[^/]+/)?(\d+)").unwrap());
// `<page>/videos/<slug?>/<id>/` — FB Page video post viewer. Same JSON shape
// as /watch?v=<id>, so route to VideoWatchParser.
static RE_PAGE_VIDEO: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/?[a-zA-Z0-9\-._]+/videos/(?:[^/]+/)?\d+").unwrap());
static RE_PHOTO: Lazy<Regex> = Lazy::new(|| Regex::new(r"^/*photo(\.php)*/*$").unwrap());
static RE_WATCH: Lazy<Regex> = Lazy::new(|| Regex::new(r"^/*watch").unwrap());
static RE_SHARE_V: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(/)?share/v/.*").unwrap());
static RE_SHARE_PR: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(/)?share/([pr]/)?[a-zA-Z0-9\-._]*(/)?").unwrap());
static RE_STORIES: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/?stories/\d+/[A-Za-z0-9=_-]+").unwrap());

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

async fn catch_all(
    State(state): State<AppState>,
    axum::extract::Path(mut path): axum::extract::Path<String>,
    raw_query: axum::extract::RawQuery,
    headers: HeaderMap,
) -> Response {
    if let Some(q) = raw_query.0 {
        if !q.is_empty() {
            path = format!("{path}?{q}");
        }
    }
    let ua = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let is_bot = crawler::is_crawler(ua);
    info!(path = %path, bot = is_bot, ua = %ua, "request");
    let started = Instant::now();

    // image-in-comment priority
    if let Ok(parsed) = Url::parse(&format!("https://www.facebook.com/{path}")) {
        let types: Vec<String> = parsed
            .query_pairs()
            .filter(|(k, _)| k == "type")
            .map(|(_, v)| v.into_owned())
            .collect();
        if types.iter().any(|t| t.contains('3')) {
            return process(&state, &path, ParserKind::Photocom).await;
        }
    }

    // crawler gate
    if !is_bot {
        let target = url_clean::ensure_absolute(&path);
        let body = format_redirect_page(&target);
        let mut hdrs = HeaderMap::new();
        hdrs.insert(
            axum::http::header::LOCATION,
            HeaderValue::from_str(&target).unwrap_or_else(|_| HeaderValue::from_static("/")),
        );
        hdrs.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        return (StatusCode::MOVED_PERMANENTLY, hdrs, body).into_response();
    }

    // share link resolve
    let mut working = path.clone();
    if RE_SHARE_V.is_match(&working) || RE_SHARE_PR.is_match(&working) {
        match resolve_share_link(&state.fetcher, &working).await {
            Ok(resolved) if !resolved.path.is_empty() => {
                working = resolved.path;
            }
            Ok(_) => {
                return html_response(format_error_embed(
                    &url_clean::ensure_absolute(&working),
                    "C",
                ));
            }
            Err(e) => return error_response(&state, &working, e),
        }
    }

    // strip tracking AFTER share resolve
    working = url_clean::clean_path(&working);
    if let Some(group_post) = group_multi_permalink_path(&working) {
        working = group_post;
    }

    // /videos/<id> → reel/<id>
    if let Some(caps) = RE_VIDEOS.captures(&working) {
        working = format!("reel/{}", &caps[1]);
    }

    // dispatch
    let kind = if RE_STORIES.is_match(&working) {
        ParserKind::Stories
    } else if RE_REEL.is_match(&working) {
        ParserKind::Reels
    } else if path_only(&working)
        .map(|p| RE_PHOTO.is_match(&p))
        .unwrap_or(false)
    {
        ParserKind::SinglePhoto
    } else if path_only(&working)
        .map(|p| RE_WATCH.is_match(&p))
        .unwrap_or(false)
    {
        ParserKind::Watch
    } else if RE_PAGE_VIDEO.is_match(&working) {
        ParserKind::Watch
    } else if is_facebook_url(&working) {
        ParserKind::JsonPost
    } else {
        return html_response(format_error_embed("https://git.facebed.com", "C"));
    };

    info!(working = %working, kind = ?kind, "dispatch");
    process_with_deadline(&state, &working, kind, started).await
}

fn path_only(s: &str) -> Option<String> {
    Url::parse(&format!(
        "https://www.facebook.com/{}",
        s.trim_start_matches('/')
    ))
    .ok()
    .map(|u| u.path().to_owned())
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
/// Returns None for shapes where the path can't identify a scope
/// (photo.php, some watch URLs) — those fall back to configured priority order.
fn scope_key(path: &str) -> Option<String> {
    let p = path_only(path)?;
    let p = p.trim_start_matches('/');
    if p.starts_with("reel/") {
        return Some("kind/reels".into());
    }
    if p.starts_with("watch") {
        return Some("kind/watch".into());
    }
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
enum ParserKind {
    JsonPost,
    SinglePhoto,
    Photocom,
    Reels,
    Watch,
    Stories,
}

async fn process(state: &AppState, path: &str, kind: ParserKind) -> Response {
    // Retry across every cookie account in configured priority order. Primary
    // account gets first chance; extra accounts are fallback/load-balancing
    // hints via affinity, not blind per-request rotation.
    //
    // Cooldown: accounts that failed recently are skipped on the first pass so
    // we don't pay a slow FB round-trip on a checkpointed/expired account
    // every other request. They're still tried as a last resort if no healthy
    // account succeeded.
    let n = state.ctx.cookies.len();
    let attempts = n.max(1);
    let mut last_err: Option<FacebedError> = None;
    let key = scope_key(path);

    // Build ordering: healthy accounts first (priority order), then
    // cooldowned ones as fallback. With n=0 (anonymous) we still loop once.
    // If we've previously seen an account succeed for this group/user, hoist
    // it to the front of the healthy list — Discord embeds expire if the
    // first try is slow, so skipping the warm-up matters here.
    let order: Vec<usize> = if n > 0 {
        let mut healthy = Vec::new();
        let mut cooled = Vec::new();
        for attempt in 0..attempts {
            let i = attempt % n;
            if state.ctx.cookies.in_cooldown(i) {
                cooled.push(i);
            } else {
                healthy.push(i);
            }
        }
        if let Some(k) = key.as_deref() {
            if let Some(pref) = state.ctx.cookies.affinity_for(k) {
                if let Some(pos) = healthy.iter().position(|&i| i == pref) {
                    let e = healthy.remove(pos);
                    healthy.insert(0, e);
                }
            }
        }
        healthy.into_iter().chain(cooled).collect()
    } else {
        vec![0]
    };

    for (loop_idx, &account_index) in order.iter().enumerate() {
        let result = if n > 0 {
            ACCOUNT_OVERRIDE
                .scope(account_index, run_parser(state, path, kind))
                .await
        } else {
            run_parser(state, path, kind).await
        };

        match result {
            Ok(post) => {
                if n > 0 {
                    state.ctx.cookies.mark_ok(account_index);
                    if let Some(k) = key.as_deref() {
                        state.ctx.cookies.set_affinity(k.to_string(), account_index);
                    }
                }
                let body = render_with_size_check(state, &post, kind).await;
                return html_response(body);
            }
            Err(e) if is_retryable(&e) && loop_idx + 1 < order.len() => {
                if n > 0 {
                    let count = state.ctx.cookies.mark_failed(account_index);
                    maybe_notify_bad_account(state, account_index, count, &e);
                    if let Some(k) = key.as_deref() {
                        if state.ctx.cookies.affinity_for(k) == Some(account_index) {
                            state.ctx.cookies.forget_affinity(k);
                        }
                    }
                }
                let label = state.ctx.cookies.label_at(account_index).unwrap_or("?");
                warn!(path = %path, attempt = loop_idx, account = %label, error = %e, "retrying with fallback account");
                last_err = Some(e);
                continue;
            }
            Err(e) => {
                if n > 0 {
                    let count = state.ctx.cookies.mark_failed(account_index);
                    maybe_notify_bad_account(state, account_index, count, &e);
                }
                return error_response(state, path, e);
            }
        }
    }

    error_response(
        state,
        path,
        last_err.unwrap_or_else(|| FacebedError::no_data(String::from("no accounts available"))),
    )
}

const DISCORD_RESPONSE_BUDGET: Duration = Duration::from_millis(5000);

async fn process_with_deadline(
    state: &AppState,
    path: &str,
    kind: ParserKind,
    started: Instant,
) -> Response {
    let elapsed = started.elapsed();
    let full_scrape = process(state, path, kind);
    tokio::pin!(full_scrape);

    if let Some(remaining) = DISCORD_RESPONSE_BUDGET.checked_sub(elapsed) {
        match tokio::time::timeout(remaining, &mut full_scrape).await {
            Ok(response) => {
                return response;
            }
            Err(_) => {
                warn!(
                    path = %path,
                    budget_ms = DISCORD_RESPONSE_BUDGET.as_millis(),
                    "full scrape exceeded Discord response budget"
                );
            }
        }
    }

    warn!(
        path = %path,
        elapsed_ms = started.elapsed().as_millis(),
        "rendering timeout embed"
    );
    no_store_html_response(format_timeout_embed(&url_clean::ensure_absolute(path)))
}

async fn run_parser(
    state: &AppState,
    path: &str,
    kind: ParserKind,
) -> Result<ParsedPost, FacebedError> {
    match kind {
        ParserKind::JsonPost => JsonPostParser.process(&state.ctx, path).await,
        ParserKind::SinglePhoto => SinglePhotoParser.process(&state.ctx, path).await,
        ParserKind::Photocom => PhotocomParser.process(&state.ctx, path).await,
        ParserKind::Reels => ReelsParser.process(&state.ctx, path).await,
        ParserKind::Watch => VideoWatchParser.process(&state.ctx, path).await,
        ParserKind::Stories => StoriesParser.process(&state.ctx, path).await,
    }
}

fn is_retryable(e: &FacebedError) -> bool {
    match e {
        FacebedError::NoData(_) | FacebedError::Parse { .. } => true,
        FacebedError::Http(err) => err.is_timeout() || err.is_connect(),
        _ => false,
    }
}

/// Discord's media proxy refuses to inline videos larger than ~25 MB, leaving
/// the user with an empty player. We HEAD the first video URL and, if FB
/// advertises a content length over this limit, fall back to a thumbnail +
/// caption + link embed instead of an `og:video`.
const DISCORD_VIDEO_BYTE_LIMIT: u64 = 25 * 1024 * 1024;

fn render(post: &ParsedPost, tz: i32, kind: ParserKind) -> String {
    // Reels/Watch always render as a video card. For mixed-media JsonPosts (video
    // + images), prefer the image-grid embed so Discord can show the photos and
    // text — Discord only renders one og:video per embed anyway, so the video
    // alone hid the rest of the post.
    let force_reel = matches!(kind, ParserKind::Reels | ParserKind::Watch);
    let video_only = !post.video_links.is_empty() && post.image_links.is_empty();
    if force_reel || video_only {
        format_reel_post_embed(post, tz)
    } else {
        format_full_post_embed(post, tz)
    }
}

async fn render_with_size_check(state: &AppState, post: &ParsedPost, kind: ParserKind) -> String {
    let tz = state.config.timezone;
    let Some(video_url) = post.video_links.first() else {
        return render(post, tz, kind);
    };
    let Some(size) = state.fetcher.head_content_length(video_url).await else {
        // Server didn't advertise Content-Length — assume it's fine and let
        // Discord try. Better to attempt the inline than silently downgrade
        // every video where FB omits the header.
        return render(post, tz, kind);
    };
    if size <= DISCORD_VIDEO_BYTE_LIMIT {
        return render(post, tz, kind);
    }
    info!(
        url = %post.url,
        bytes = size,
        limit = DISCORD_VIDEO_BYTE_LIMIT,
        "video oversized for Discord media proxy — falling back to thumbnail embed"
    );
    format_oversized_video_embed(post, tz)
}

/// Fire a Discord webhook when an account has failed [`NOTIFY_FAILURE_THRESHOLD`]
/// times in a row. Resets the counter afterwards so the next bad streak
/// re-alerts instead of spamming on every subsequent failure.
fn maybe_notify_bad_account(state: &AppState, account_index: usize, count: u64, e: &FacebedError) {
    if count != NOTIFY_FAILURE_THRESHOLD {
        return;
    }
    let label = state
        .ctx
        .cookies
        .label_at(account_index)
        .unwrap_or("?")
        .to_owned();
    let msg = format!(
        "@everyone account `{label}` failed {count}× in a row — cookie likely expired or checkpointed. \
         Please re-export and update `cookies-{label}.json`. Last error: `{e}`"
    );
    warn!(account = %label, count, "notifying admin about bad account");
    state.notifier.warn(msg, None);
    state.ctx.cookies.reset_failure_count(account_index);
}

fn error_response(state: &AppState, path: &str, e: FacebedError) -> Response {
    let url = url_clean::ensure_absolute(path);
    let code = e.error_code();
    match &e {
        FacebedError::NoData(msg) => {
            info!(path = %path, "no data: {}", msg);
        }
        FacebedError::Parse {
            message,
            html,
            url: u,
        } => {
            error!(path = %path, error = %message, "parser bug");
            let page_url = u.clone().unwrap_or_else(|| url.clone());
            let warn_msg = format!("🚨 **ParseException** for `{path}`\n{page_url}\n`{message}`");
            if let Some(h) = html {
                let safe = path
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                    .take(80)
                    .collect::<String>();
                state.notifier.warn(
                    warn_msg,
                    Some((format!("{safe}.html"), h.clone().into_bytes())),
                );
            } else {
                state.notifier.warn(warn_msg, None);
            }
        }
        _ => {
            warn!(path = %path, error = %e, "unclassified error");
        }
    }
    html_response(format_error_embed(&url, code))
}

#[cfg(test)]
mod tests {
    use super::{group_multi_permalink_path, is_facebook_url, scope_key};

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
    fn video_routes_share_kind_affinity() {
        assert_eq!(scope_key("reel/12345"), Some("kind/reels".into()));
        assert_eq!(scope_key("watch?v=12345"), Some("kind/watch".into()));
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
