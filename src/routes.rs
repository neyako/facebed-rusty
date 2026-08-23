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
    comment::CommentParser, json_post::JsonPostParser, photocom::PhotocomParser,
    reels::ReelsParser, resolve_facebook_author_handle, single_photo::SinglePhotoParser,
    stories::StoriesParser, video_watch::VideoWatchParser, ParsedPost, Parser, ParserCtx,
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

#[derive(Default)]
pub struct Metrics {
    pub requests: std::sync::atomic::AtomicU64,
    pub errors: std::sync::atomic::AtomicU64,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<arc_swap::ArcSwap<Config>>,
    pub ctx: Arc<ParserCtx>,
    pub notifier: Notifier,
    pub fetcher: Arc<Fetcher>,
    pub embed_cache: Arc<std::sync::Mutex<crate::embed_cache::EmbedCache>>,
    pub fetch_limit: Arc<tokio::sync::Semaphore>,
    pub metrics: Arc<Metrics>,
    pub started_at: Instant,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/oembed.json", get(oembed))
        .route("/favicon.ico", get(favicon))
        .route("/banner.png", get(banner))
        .route("/healthz", get(healthz))
        .route("/media", get(media))
        .route("/api/v1/statuses/:id", get(activity_status))
        .route("/users/:username/statuses/:id", get(user_activity_status))
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

fn json_response(body: String) -> Response {
    json_status_response(StatusCode::OK, body)
}

fn json_status_response(status: StatusCode, body: String) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    (status, headers, body).into_response()
}

fn activity_error_response(status: StatusCode) -> Response {
    let mut response = json_status_response(
        status,
        serde_json::json!({
            "error": status.canonical_reason().unwrap_or("error"),
        })
        .to_string(),
    );
    if status == StatusCode::SERVICE_UNAVAILABLE {
        response.headers_mut().insert(
            axum::http::header::RETRY_AFTER,
            HeaderValue::from_static("2"),
        );
    }
    response
}

fn busy_response() -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::RETRY_AFTER,
        HeaderValue::from_static("2"),
    );
    (StatusCode::SERVICE_UNAVAILABLE, headers, "busy").into_response()
}

#[derive(serde::Deserialize)]
struct OEmbedParams {
    #[serde(default)]
    author: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default, rename = "type")]
    kind: String,
}

async fn oembed(axum::extract::Query(p): axum::extract::Query<OEmbedParams>) -> Response {
    json_response(build_oembed_json(&p.author, &p.title, &p.url, &p.kind))
}

async fn activity_status(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    activity_status_for_id(state, id).await
}

async fn user_activity_status(
    State(state): State<AppState>,
    axum::extract::Path((_username, id)): axum::extract::Path<(String, String)>,
) -> Response {
    activity_status_for_id(state, id).await
}

async fn activity_status_for_id(state: AppState, id: String) -> Response {
    let (path, kind) = match activity_path(&id) {
        Ok(activity) => activity,
        Err(status) => return activity_error_response(status),
    };

    if let Ok(mut cache) = state.embed_cache.lock() {
        if let Some(post) = cache.get_activity(&id, Instant::now()) {
            return json_response(crate::activity::status_json(&id, &post));
        }
    }

    let _permit = match state.fetch_limit.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return activity_error_response(StatusCode::SERVICE_UNAVAILABLE),
    };
    let post = match tokio::time::timeout(DISCORD_RESPONSE_BUDGET, run_parser(&state, &path, kind))
        .await
    {
        Ok(Ok(post)) => post,
        Ok(Err(_)) => return activity_error_response(StatusCode::NOT_FOUND),
        Err(_) => return activity_error_response(StatusCode::SERVICE_UNAVAILABLE),
    };

    if let Ok(mut cache) = state.embed_cache.lock() {
        cache.insert_activity(&id, post.clone(), Instant::now());
    }
    json_response(crate::activity::status_json(&id, &post))
}

#[derive(serde::Deserialize)]
struct MediaParams {
    #[serde(default)]
    u: String,
}

const MAX_MEDIA_BYTES: u64 = 30 * 1024 * 1024;

async fn media(
    State(state): State<AppState>,
    axum::extract::Query(p): axum::extract::Query<MediaParams>,
) -> Response {
    let Ok(parsed) = Url::parse(&p.u) else {
        return (StatusCode::BAD_REQUEST, "bad url").into_response();
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return (StatusCode::BAD_REQUEST, "bad scheme").into_response();
    }
    if !media_target_allowed(&p.u) {
        return (StatusCode::FORBIDDEN, "host not allowed").into_response();
    }

    let upstream = match state.fetcher.media_client().get(parsed).send().await {
        Ok(r) => r,
        Err(_) => return (StatusCode::BAD_GATEWAY, "upstream error").into_response(),
    };
    if !upstream.status().is_success() {
        return (StatusCode::BAD_GATEWAY, "upstream status").into_response();
    }
    if let Some(len) = upstream.content_length() {
        if len > MAX_MEDIA_BYTES {
            return (StatusCode::PAYLOAD_TOO_LARGE, "too large").into_response();
        }
    }

    let ct = upstream
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("application/octet-stream"));
    let mut headers = HeaderMap::new();
    headers.insert(axum::http::header::CONTENT_TYPE, ct);
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    let body = axum::body::Body::from_stream(upstream.bytes_stream());
    (StatusCode::OK, headers, body).into_response()
}

/// Returns true iff `u` is a fetchable Facebook media URL.
fn media_target_allowed(u: &str) -> bool {
    match Url::parse(u) {
        Ok(parsed) => {
            matches!(parsed.scheme(), "http" | "https")
                && parsed
                    .host_str()
                    .map(crate::url_clean::is_facebook_media_host)
                    .unwrap_or(false)
        }
        Err(_) => false,
    }
}

async fn healthz(State(state): State<AppState>) -> Response {
    use std::sync::atomic::Ordering::Relaxed;

    let jar = state.ctx.cookies.load();
    let accounts: Vec<(String, bool)> = (0..jar.len())
        .map(|i| {
            (
                jar.label_at(i).unwrap_or("?").to_string(),
                jar.in_cooldown(i),
            )
        })
        .collect();
    let body = build_healthz_json(
        state.started_at.elapsed().as_secs(),
        state.metrics.requests.load(Relaxed),
        state.metrics.errors.load(Relaxed),
        &accounts,
    );
    json_response(body)
}

/// Build the oEmbed 1.0 document Discord reads to render the author/provider
/// line. Kept pure (no extractors) so it is unit-testable.
fn build_oembed_json(engagement: &str, title: &str, url: &str, kind: &str) -> String {
    let kind = match kind {
        "video" | "photo" | "rich" => kind,
        _ => "link",
    };
    serde_json::json!({
        "version": "1.0",
        "type": kind,
        "provider_name": crate::embed::credit(),
        "provider_url": url,
        "author_name": engagement,
        "author_url": url,
        "title": title,
    })
    .to_string()
}

fn build_healthz_json(
    uptime_secs: u64,
    requests: u64,
    errors: u64,
    accounts: &[(String, bool)],
) -> String {
    let accounts: Vec<serde_json::Value> = accounts
        .iter()
        .map(|(label, in_cooldown)| serde_json::json!({"label": label, "in_cooldown": in_cooldown}))
        .collect();
    serde_json::json!({
        "status": "ok",
        "uptime_secs": uptime_secs,
        "requests": requests,
        "errors": errors,
        "cookie_accounts": accounts.len(),
        "accounts": accounts,
    })
    .to_string()
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

static RE_REEL: Lazy<Regex> = Lazy::new(|| Regex::new(r"^/?reel/[0-9]+/?(?:\?.*)?$").unwrap());
static RE_REEL_TWO_SEGMENTS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/?reel/[0-9]+/[0-9]+/?$").unwrap());
// Only match bare `videos/<id>` (no Page prefix). Page-scoped video posts like
// `<page>/videos/<slug>/<id>` are real video viewer pages — not reels — and
// FB serves them with a watch-style JSON shape that the JsonPost root walker
// can't handle. Routed below to VideoWatchParser via [`RE_PAGE_VIDEO`].
static RE_VIDEOS: Lazy<Regex> = Lazy::new(|| Regex::new(r"^/?videos/(?:[^/]+/)?(\d+)").unwrap());
// `<page>/videos/<slug?>/<id>/` — FB Page video post viewer. Same JSON shape
// as /watch?v=<id>, so route to VideoWatchParser.
static RE_PAGE_VIDEO: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/?[a-zA-Z0-9\-._]+/videos/(?:[^/]+/)?\d+").unwrap());
static RE_SLUGGED_PHOTO: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/?[a-zA-Z0-9\-._]+/photos/[^/?]+/(\d+)/?(?:\?.*)?$").unwrap());
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
    let activity_origin = request_origin(&headers);
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
            let cleaned = url_clean::clean_path(&path);
            return process(
                &state,
                PostRequest {
                    path: &cleaned,
                    kind: ParserKind::Photocom,
                    activity_origin: activity_origin.as_deref(),
                },
            )
            .await;
        }
    }

    // crawler gate
    if !is_bot {
        let target = url_clean::ensure_absolute(&path);
        let target = if url_clean::is_facebook_page_url(&target) {
            target
        } else {
            String::from("https://www.facebook.com/")
        };
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
    if let Some(wrapped) = url_clean::extract_share_url(&working) {
        working = wrapped;
    }
    if RE_SHARE_V.is_match(&working) || RE_SHARE_PR.is_match(&working) {
        let remaining = match DISCORD_RESPONSE_BUDGET.checked_sub(started.elapsed()) {
            Some(remaining) => remaining,
            None => {
                warn!(
                    path = %working,
                    elapsed_ms = started.elapsed().as_millis(),
                    "share resolve skipped after Discord response budget"
                );
                return no_store_html_response(format_timeout_embed(&url_clean::ensure_absolute(
                    &working,
                )));
            }
        };
        match tokio::time::timeout(remaining, resolve_share_link(&state.fetcher, &working)).await {
            Err(_) => {
                warn!(
                    path = %working,
                    budget_ms = DISCORD_RESPONSE_BUDGET.as_millis(),
                    "share resolve exceeded Discord response budget"
                );
                return no_store_html_response(format_timeout_embed(&url_clean::ensure_absolute(
                    &working,
                )));
            }
            Ok(Ok(resolved)) if !resolved.path.is_empty() => {
                working = resolved.path;
            }
            Ok(Ok(_)) => {
                return html_response(format_error_embed(
                    &url_clean::ensure_absolute(&working),
                    "C",
                ));
            }
            Ok(Err(e)) => return error_response(&state, &working, e),
        }
    }

    // strip tracking AFTER share resolve
    working = url_clean::clean_path(&working);
    if let Some(group_post) = group_multi_permalink_path(&working) {
        working = group_post;
    }
    if let Some(rewritten) = rewrite_slugged_photo_path(&working) {
        working = rewritten;
    }

    // /videos/<id> → reel/<id>
    if let Some(rewritten) = rewrite_videos_path(&working) {
        working = rewritten;
    }
    if let Some(normalized) = normalize_reel_path(&working) {
        working = normalized;
    }

    // dispatch — comment permalinks first, then path-shape routing
    let kind = if crate::parsers::comment::comment_id_in(&working).is_some() {
        ParserKind::Comment
    } else {
        match select_kind(&working) {
            Some(kind) => kind,
            None => return html_response(format_error_embed("https://git.facebed.com", "C")),
        }
    };

    info!(working = %working, kind = ?kind, "dispatch");
    process_with_deadline(
        &state,
        PostRequest {
            path: &working,
            kind,
            activity_origin: activity_origin.as_deref(),
        },
        started,
    )
    .await
}

fn request_origin(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(axum::http::header::HOST)?.to_str().ok()?.trim();
    let mut forwarded_proto = headers.get_all("x-forwarded-proto").iter();
    let scheme = match forwarded_proto.next() {
        Some(value) => {
            if forwarded_proto.next().is_some() {
                return None;
            }
            value.to_str().ok()?.trim()
        }
        None => "https",
    };
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    let parsed = Url::parse(&format!("{scheme}://{host}")).ok()?;
    if parsed.host_str().is_none()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return None;
    }
    Some(parsed.origin().ascii_serialization())
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

fn select_kind(working: &str) -> Option<ParserKind> {
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

fn activity_path(id: &str) -> Result<(String, ParserKind), StatusCode> {
    let mut path = crate::activity::decode_status_path(id).ok_or(StatusCode::BAD_REQUEST)?;
    if let Some(group_post) = group_multi_permalink_path(&path) {
        path = group_post;
    }
    if let Some(rewritten) = rewrite_slugged_photo_path(&path) {
        path = rewritten;
    }
    if let Some(rewritten) = rewrite_videos_path(&path) {
        path = rewritten;
    }
    if let Some(normalized) = normalize_reel_path(&path) {
        path = normalized;
    }

    let is_photocom = Url::parse(&url_clean::ensure_absolute(&path))
        .ok()
        .map(|url| {
            url.query_pairs()
                .any(|(key, value)| key == "type" && value.contains('3'))
        })
        .unwrap_or(false);
    if is_photocom {
        return Ok((path, ParserKind::Photocom));
    }
    if crate::parsers::comment::comment_id_in(&path).is_some() {
        return Ok((path, ParserKind::Comment));
    }

    match select_kind(&path) {
        Some(kind) => Ok((path, kind)),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Drop comment_id/reply_comment_id so a failed comment lookup can re-dispatch
/// as the plain post/video it hangs off.
fn strip_comment_id(path: &str) -> String {
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
    Comment,
}

#[derive(Clone, Copy)]
struct PostRequest<'a> {
    path: &'a str,
    kind: ParserKind,
    activity_origin: Option<&'a str>,
}

impl PostRequest<'_> {
    fn cache_key(&self) -> String {
        self.activity_origin.map_or_else(
            || self.path.to_owned(),
            |origin| format!("{origin}\n{}", self.path),
        )
    }
}

async fn process(state: &AppState, request: PostRequest<'_>) -> Response {
    let path = request.path;
    let kind = request.kind;
    let cache_key = request.cache_key();
    state
        .metrics
        .requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    {
        let now = Instant::now();
        if let Ok(mut cache) = state.embed_cache.lock() {
            if let Some(body) = cache.get(&cache_key, now) {
                info!(path = %path, cached = true, "embed cache hit");
                return html_response(body);
            }
        }
    }

    let _permit = match state.fetch_limit.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return busy_response(),
    };

    // Retry across every cookie account in configured priority order. Primary
    // account gets first chance; extra accounts are fallback/load-balancing
    // hints via affinity, not blind per-request rotation.
    //
    // Cooldown: accounts that failed recently are skipped on the first pass so
    // we don't pay a slow FB round-trip on a checkpointed/expired account
    // every other request. They're still tried as a last resort if no healthy
    // account succeeded.
    let n = state.ctx.cookies.load().len();
    let attempts = n.max(1);
    let mut last_err: Option<FacebedError> = None;
    let key = scope_key(path);
    let process_started = Instant::now();

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
            if state.ctx.cookies.load().in_cooldown(i) {
                cooled.push(i);
            } else {
                healthy.push(i);
            }
        }
        if let Some(k) = key.as_deref() {
            if let Some(pref) = state.ctx.cookies.load().affinity_for(k) {
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
        let attempt_started = Instant::now();
        let result = if n > 0 {
            ACCOUNT_OVERRIDE
                .scope(account_index, run_parser(state, path, kind))
                .await
        } else {
            run_parser(state, path, kind).await
        };

        match result {
            Ok(post) => {
                let scrape_ms = attempt_started.elapsed().as_millis();
                if n > 0 {
                    state.ctx.cookies.load().mark_ok(account_index);
                    if let Some(k) = key.as_deref() {
                        state
                            .ctx
                            .cookies
                            .load()
                            .set_affinity(k.to_string(), account_index);
                    }
                }
                let render_started = Instant::now();
                let body = render_with_size_check(state, &post, request).await;
                if let Ok(mut cache) = state.embed_cache.lock() {
                    if activity_eligible(&post) {
                        if let Some(id) = crate::activity::status_id(&post.url) {
                            cache.insert_activity(&id, post.clone(), Instant::now());
                        }
                    }
                    cache.insert(&cache_key, body.clone(), Instant::now());
                }
                info!(
                    path = %path,
                    kind = ?kind,
                    account = %state.ctx.cookies.load().label_at(account_index).unwrap_or(""),
                    attempt = loop_idx,
                    scrape_ms,
                    render_ms = render_started.elapsed().as_millis(),
                    total_ms = process_started.elapsed().as_millis(),
                    "embed rendered"
                );
                return html_response(body);
            }
            Err(e) if is_retryable(&e) && loop_idx + 1 < order.len() => {
                if n > 0 {
                    record_account_failure(state, account_index, &e, key.as_deref());
                }
                let guard = state.ctx.cookies.load();
                let label = guard.label_at(account_index).unwrap_or("?");
                warn!(path = %path, attempt = loop_idx, account = %label, error = %e, elapsed_ms = attempt_started.elapsed().as_millis(), "retrying with fallback account");
                last_err = Some(e);
                continue;
            }
            Err(e) => {
                if n > 0 {
                    record_account_failure(state, account_index, &e, key.as_deref());
                }
                warn!(
                    path = %path,
                    kind = ?kind,
                    account = %state.ctx.cookies.load().label_at(account_index).unwrap_or(""),
                    attempt = loop_idx,
                    error = %e,
                    attempt_ms = attempt_started.elapsed().as_millis(),
                    total_ms = process_started.elapsed().as_millis(),
                    "embed render failed"
                );
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

// Discord's embed crawler aborts ~10.0s after fetch start (measured 2026-07-16
// by bisecting delayed-OG responses in a live channel: <=9.4s rendered every
// time, 9.5-9.6s was flaky, >=9.7s never rendered). 8500ms keeps the whole
// response inside the reliable zone with margin for edge/origin latency.
const DISCORD_RESPONSE_BUDGET: Duration = Duration::from_millis(8500);

async fn process_with_deadline(
    state: &AppState,
    request: PostRequest<'_>,
    started: Instant,
) -> Response {
    let path = request.path;
    let elapsed = started.elapsed();
    let full_scrape = process(state, request);
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
    let post = match kind {
        ParserKind::JsonPost => JsonPostParser.process(&state.ctx, path).await,
        ParserKind::SinglePhoto => SinglePhotoParser.process(&state.ctx, path).await,
        ParserKind::Photocom => PhotocomParser.process(&state.ctx, path).await,
        ParserKind::Reels => ReelsParser.process(&state.ctx, path).await,
        ParserKind::Watch => VideoWatchParser.process(&state.ctx, path).await,
        ParserKind::Stories => StoriesParser.process(&state.ctx, path).await,
        ParserKind::Comment => {
            match CommentParser.process(&state.ctx, path).await {
                Err(FacebedError::NoData(reason)) => {
                    // Comment absent from SSR HTML — embed the underlying post
                    // instead of erroring with C.
                    let stripped = strip_comment_id(path);
                    let Some(fallback) = select_kind(&stripped) else {
                        return Err(FacebedError::no_data(reason));
                    };
                    info!(
                        path = %stripped,
                        kind = ?fallback,
                        %reason,
                        "comment not found; falling back to post embed"
                    );
                    Box::pin(run_parser(state, &stripped, fallback)).await
                }
                other => other,
            }
        }
    }?;
    Ok(resolve_facebook_author_handle(&state.ctx, post).await)
}

fn is_retryable(e: &FacebedError) -> bool {
    match e {
        FacebedError::NoData(_)
        | FacebedError::Parse { .. }
        | FacebedError::RateLimited { .. }
        | FacebedError::Checkpointed => true,
        FacebedError::Http(err) => {
            err.is_timeout() || err.is_connect() || err.is_decode()
        }
        _ => false,
    }
}

/// Discord's media proxy refuses to inline videos larger than ~25 MB, leaving
/// the user with an empty player. We HEAD the first video URL and, if FB
/// advertises a content length over this limit, fall back to a thumbnail +
/// caption + link embed instead of an `og:video`.
const DISCORD_VIDEO_BYTE_LIMIT: u64 = 25 * 1024 * 1024;

fn activity_eligible(post: &ParsedPost) -> bool {
    crate::activity::status_id(&post.url).is_some()
}

fn render(post: &ParsedPost, tz: i32, request: PostRequest<'_>) -> String {
    let kind = request.kind;
    let activity_origin = request.activity_origin.filter(|_| activity_eligible(post));
    // Reels/Watch always render as a video card. For mixed-media JsonPosts (video
    // + images), prefer the image-grid embed so Discord can show the photos and
    // text — Discord only renders one og:video per embed anyway, so the video
    // alone hid the rest of the post.
    let force_reel = matches!(kind, ParserKind::Reels | ParserKind::Watch);
    let video_only = !post.video_links.is_empty() && post.image_links.is_empty();
    if force_reel || video_only {
        format_reel_post_embed(post, tz, activity_origin)
    } else {
        format_full_post_embed(post, tz, activity_origin)
    }
}

async fn render_with_size_check(
    state: &AppState,
    post: &ParsedPost,
    request: PostRequest<'_>,
) -> String {
    let tz = state.config.load().timezone;
    let Some(video_url) = post.video_links.first() else {
        return render(post, tz, request);
    };
    let Some(size) = state.fetcher.head_content_length(video_url).await else {
        // Server didn't advertise Content-Length — assume it's fine and let
        // Discord try. Better to attempt the inline than silently downgrade
        // every video where FB omits the header.
        info!(
            url = %post.url,
            "video size unavailable; rendering inline video embed"
        );
        return render(post, tz, request);
    };
    if size <= DISCORD_VIDEO_BYTE_LIMIT {
        return render(post, tz, request);
    }
    info!(
        url = %post.url,
        bytes = size,
        limit = DISCORD_VIDEO_BYTE_LIMIT,
        "video oversized for Discord media proxy — falling back to thumbnail embed"
    );
    let activity_origin = request.activity_origin.filter(|_| activity_eligible(post));
    format_oversized_video_embed(post, tz, activity_origin)
}

/// Apply cooldown and alerting appropriate to a failed attempt's cause.
fn record_account_failure(
    state: &AppState,
    account_index: usize,
    e: &FacebedError,
    key: Option<&str>,
) {
    match e {
        FacebedError::RateLimited { retry_after } => {
            state
                .ctx
                .cookies
                .load()
                .mark_rate_limited(account_index, *retry_after);
            return;
        }
        FacebedError::Checkpointed => {
            let count = state.ctx.cookies.load().mark_checkpointed(account_index);
            maybe_notify_bad_account(state, account_index, count, e);
        }
        _ => {
            let count = state.ctx.cookies.load().mark_failed(account_index);
            maybe_notify_bad_account(state, account_index, count, e);
        }
    }
    if let Some(k) = key {
        if state.ctx.cookies.load().affinity_for(k) == Some(account_index) {
            state.ctx.cookies.load().forget_affinity(k);
        }
    }
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
        .load()
        .label_at(account_index)
        .unwrap_or("?")
        .to_owned();
    let msg = format!(
        "@everyone account `{label}` failed {count}× in a row — cookie likely expired or checkpointed. \
         Please re-export and update `cookies-{label}.json`. Last error: `{e}`"
    );
    warn!(account = %label, count, "notifying admin about bad account");
    state.notifier.warn(msg, None);
    state.ctx.cookies.load().reset_failure_count(account_index);
}

fn error_response(state: &AppState, path: &str, e: FacebedError) -> Response {
    state
        .metrics
        .errors
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

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
    use super::{
        activity_eligible, activity_error_response, activity_path, build_healthz_json,
        build_oembed_json, group_multi_permalink_path, is_facebook_url, media_target_allowed,
        normalize_reel_path, request_origin, rewrite_videos_path, router, scope_key, select_kind,
        strip_comment_id, AppState, Metrics, ParserKind,
    };
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request, StatusCode};
    use std::sync::Arc;
    use std::time::Instant;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let config = Arc::new(arc_swap::ArcSwap::from_pointee(
            crate::config::Config::default(),
        ));
        let cookies = Arc::new(arc_swap::ArcSwap::from_pointee(
            crate::cookies::CookieJar::empty(),
        ));
        let fetcher = Arc::new(crate::fetch::Fetcher::new(cookies.clone()).expect("test fetcher"));
        let ctx = Arc::new(crate::parsers::ParserCtx {
            fetcher: fetcher.clone(),
            cookies,
            config: config.clone(),
        });

        AppState {
            config,
            ctx,
            notifier: crate::notifier::Notifier::new(String::new(), fetcher.client().clone()),
            fetcher,
            embed_cache: Arc::new(std::sync::Mutex::new(
                crate::embed_cache::EmbedCache::default(),
            )),
            fetch_limit: Arc::new(tokio::sync::Semaphore::new(0)),
            metrics: Arc::new(Metrics::default()),
            started_at: Instant::now(),
        }
    }

    #[tokio::test]
    async fn activity_alias_returns_cached_status_json_when_preloaded() {
        // Given
        let state = test_state();
        let post = activity_post();
        let id = crate::activity::status_id(&post.url).expect("activity status id");
        let expected = crate::activity::status_json(&id, &post);
        state
            .embed_cache
            .lock()
            .expect("activity cache")
            .insert_activity(&id, post, Instant::now());
        let request = Request::builder()
            .uri(format!("/users/example.author/statuses/{id}"))
            .body(Body::empty())
            .expect("activity request");

        // When
        let response = router(state)
            .oneshot(request)
            .await
            .expect("route response");

        // Then
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&header::HeaderValue::from_static(
                "application/json; charset=utf-8"
            ))
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("activity response body");
        assert_eq!(&body[..], expected.as_bytes());
    }

    #[tokio::test]
    async fn activity_alias_returns_cached_video_status_json_when_preloaded() {
        // Given
        let state = test_state();
        let mut post = activity_post();
        post.video_links = vec!["https://video.example/post.mp4".into()];
        post.thumbnail = Some("https://img.example/post.jpg".into());
        let id = crate::activity::status_id(&post.url).expect("activity status id");
        let expected = crate::activity::status_json(&id, &post);
        state
            .embed_cache
            .lock()
            .expect("activity cache")
            .insert_activity(&id, post, Instant::now());
        let request = Request::builder()
            .uri(format!("/users/example.author/statuses/{id}"))
            .body(Body::empty())
            .expect("activity request");

        // When
        let response = router(state)
            .oneshot(request)
            .await
            .expect("route response");

        // Then
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("activity response body");
        assert_eq!(&body[..], expected.as_bytes());
    }

    #[tokio::test]
    async fn bot_request_selects_matching_origin_over_other_and_legacy_cache() {
        // Given
        let state = test_state();
        let path = "groups/example/posts/123";
        {
            let mut cache = state.embed_cache.lock().expect("embed cache");
            cache.insert(
                &format!("https://origin-a.example\n{path}"),
                "origin-a cached HTML".into(),
                Instant::now(),
            );
            cache.insert(
                &format!("https://origin-b.example\n{path}"),
                "origin-b cached HTML".into(),
                Instant::now(),
            );
            cache.insert(path, "legacy cached HTML".into(), Instant::now());
        }
        let request = Request::builder()
            .uri(format!("/{path}"))
            .header(header::HOST, "origin-b.example")
            .header("x-forwarded-proto", "https")
            .header(header::USER_AGENT, "Discordbot/2.0")
            .body(Body::empty())
            .expect("bot request");

        // When
        let response = router(state)
            .oneshot(request)
            .await
            .expect("route response");

        // Then
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("cached response body");
        assert_eq!(&body[..], b"origin-b cached HTML");
    }

    #[tokio::test]
    async fn slugged_photo_request_uses_photo_php_cache_key() {
        let state = test_state();
        let normalized = "photo.php?fbid=29046668624922185&set=a.228654573816974&hpir=1";
        state.embed_cache.lock().expect("embed cache").insert(
            normalized,
            "slugged photo cached HTML".into(),
            Instant::now(),
        );
        let request = Request::builder()
            .uri("/shinantori/photos/claude-code-output-style/29046668624922185/?set=a.228654573816974&hpir=1")
            .header(header::USER_AGENT, "Discordbot/2.0")
            .body(Body::empty())
            .expect("slugged photo request");

        let response = router(state)
            .oneshot(request)
            .await
            .expect("route response");
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("cached response body");

        assert_eq!(&body[..], b"slugged photo cached HTML");
    }

    #[test]
    fn request_origin_uses_forwarded_https_host() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("facebed.example"),
        );
        headers.insert(
            "x-forwarded-proto",
            axum::http::HeaderValue::from_static("https"),
        );

        assert_eq!(
            request_origin(&headers).as_deref(),
            Some("https://facebed.example")
        );
    }

    #[test]
    fn request_origin_rejects_authority_with_userinfo_path_or_query() {
        for host in [
            "attacker@facebed.example",
            "facebed.example/hidden",
            "facebed.example?next=hidden",
        ] {
            // Given
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                axum::http::header::HOST,
                axum::http::HeaderValue::from_static(host),
            );

            // When
            let origin = request_origin(&headers);

            // Then
            assert_eq!(origin, None, "host must be rejected: {host}");
        }
    }

    #[test]
    fn request_origin_rejects_unsupported_forwarded_proto() {
        // Given
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("facebed.example"),
        );
        headers.insert(
            "x-forwarded-proto",
            axum::http::HeaderValue::from_static("ftp"),
        );

        // When
        let origin = request_origin(&headers);

        // Then
        assert_eq!(origin, None);
    }

    #[test]
    fn request_origin_rejects_multi_valued_forwarded_proto() {
        // Given
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::HOST,
            axum::http::HeaderValue::from_static("facebed.example"),
        );
        headers.append(
            "x-forwarded-proto",
            axum::http::HeaderValue::from_static("https"),
        );
        headers.append(
            "x-forwarded-proto",
            axum::http::HeaderValue::from_static("http"),
        );

        // When
        let origin = request_origin(&headers);

        // Then
        assert_eq!(origin, None);
    }

    fn activity_post() -> crate::parsers::ParsedPost {
        crate::parsers::ParsedPost {
            author_name: "Author".into(),
            author_id: None,
            author_handle: Some("example.author".into()),
            author_avatar_url: None,
            context: None,
            text: "Post".into(),
            allow_discord_markdown: false,
            image_links: vec!["https://img.example/post.jpg".into()],
            url: "https://www.facebook.com/groups/example/posts/123".into(),
            date: 0,
            likes: "null".into(),
            top_reaction_ids: Vec::new(),
            comments: "null".into(),
            shares: "null".into(),
            video_links: Vec::new(),
            thumbnail: None,
        }
    }

    #[test]
    fn activity_eligibility_accepts_facebook_posts_with_any_media_type() {
        let post = activity_post();

        assert!(activity_eligible(&post));

        let mut mixed = post;
        mixed
            .video_links
            .push("https://video.example/post.mp4".into());
        assert!(activity_eligible(&mixed));

        mixed.url = "https://example.com/not-facebook".into();
        assert!(!activity_eligible(&mixed));
    }

    #[test]
    fn activity_service_unavailable_retries_after_two_seconds() {
        let response = activity_error_response(axum::http::StatusCode::SERVICE_UNAVAILABLE);

        assert_eq!(
            response.headers().get(axum::http::header::RETRY_AFTER),
            Some(&axum::http::HeaderValue::from_static("2"))
        );
        assert_eq!(
            response.headers().get(axum::http::header::CONTENT_TYPE),
            Some(&axum::http::HeaderValue::from_static(
                "application/json; charset=utf-8"
            ))
        );
    }

    #[test]
    fn activity_path_rejects_malformed_and_unsupported_ids() {
        assert!(matches!(
            activity_path("12x"),
            Err(axum::http::StatusCode::BAD_REQUEST)
        ));

        let marketplace =
            crate::activity::status_id("https://www.facebook.com/marketplace/item/123").unwrap();
        assert!(matches!(
            activity_path(&marketplace),
            Err(axum::http::StatusCode::NOT_FOUND)
        ));
    }

    #[test]
    fn activity_path_accepts_every_supported_parser_kind() {
        let group = crate::activity::status_id("https://www.facebook.com/groups/example/posts/123")
            .unwrap();
        let photo =
            crate::activity::status_id("https://www.facebook.com/photo.php?fbid=123&id=456")
                .unwrap();
        let photocom =
            crate::activity::status_id("https://www.facebook.com/photo.php?fbid=123&id=456&type=3")
                .unwrap();
        let reel = crate::activity::status_id("https://www.facebook.com/reel/123").unwrap();
        let watch = crate::activity::status_id("https://www.facebook.com/watch?v=123").unwrap();
        let story = crate::activity::status_id("https://www.facebook.com/stories/123/abc").unwrap();
        let comment = crate::activity::status_id(
            "https://www.facebook.com/groups/example/posts/123?comment_id=456",
        )
        .unwrap();

        assert!(matches!(
            activity_path(&group),
            Ok((path, ParserKind::JsonPost)) if path == "groups/example/posts/123"
        ));
        assert!(matches!(
            activity_path(&photo),
            Ok((path, ParserKind::SinglePhoto)) if path == "photo.php?fbid=123&id=456"
        ));
        assert!(matches!(
            activity_path(&photocom),
            Ok((path, ParserKind::Photocom))
                if path == "photo.php?fbid=123&id=456&type=3"
        ));
        assert!(matches!(
            activity_path(&reel),
            Ok((path, ParserKind::Reels)) if path == "reel/123"
        ));
        assert!(matches!(
            activity_path(&watch),
            Ok((path, ParserKind::Watch)) if path == "watch?v=123"
        ));
        assert!(matches!(
            activity_path(&story),
            Ok((path, ParserKind::Stories)) if path == "stories/123/abc"
        ));
        assert!(matches!(
            activity_path(&comment),
            Ok((path, ParserKind::Comment))
                if path == "groups/example/posts/123?comment_id=456"
        ));
    }

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
    fn activity_path_keeps_query_when_normalizing_two_segment_reel() {
        let id = crate::activity::status_id(
            "https://www.facebook.com/reel/1376968477584004/1013234327723021?x=1",
        )
        .expect("activity id");

        assert!(matches!(
            activity_path(&id),
            Ok((path, ParserKind::Reels)) if path == "reel/1013234327723021?x=1"
        ));
    }

    #[test]
    fn activity_path_normalizes_two_segment_reel_before_dispatch() {
        let id = crate::activity::status_id(
            "https://www.facebook.com/reel/1376968477584004/1013234327723021",
        )
        .expect("activity id");

        assert!(matches!(
            activity_path(&id),
            Ok((path, ParserKind::Reels)) if path == "reel/1013234327723021"
        ));
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
    fn video_routes_share_kind_affinity() {
        assert_eq!(scope_key("reel/12345"), Some("kind/reels".into()));
        assert_eq!(scope_key("watch?v=12345"), Some("kind/watch".into()));
    }

    #[test]
    fn rate_limit_and_checkpoint_are_retryable() {
        use crate::error::FacebedError;

        assert!(super::is_retryable(&FacebedError::rate_limited(Some(30))));
        assert!(super::is_retryable(&FacebedError::checkpointed()));
    }

    #[test]
    fn fetch_cap_rejects_when_exhausted() {
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(2));
        let _a = sem.clone().try_acquire_owned().expect("first permit");
        let _b = sem.clone().try_acquire_owned().expect("second permit");
        assert!(
            sem.clone().try_acquire_owned().is_err(),
            "third acquire must fail when the 2-permit cap is exhausted"
        );
        drop(_a);
        assert!(
            sem.clone().try_acquire_owned().is_ok(),
            "a permit frees up after one is dropped"
        );
    }

    #[test]
    fn healthz_json_reports_counters_and_accounts() {
        let json = build_healthz_json(42, 7, 1, &[("primary".into(), true), ("alt".into(), false)]);
        assert!(json.contains(r#""status":"ok""#));
        assert!(json.contains(r#""uptime_secs":42"#));
        assert!(json.contains(r#""requests":7"#));
        assert!(json.contains(r#""errors":1"#));
        assert!(json.contains(r#""cookie_accounts":2"#));
        assert!(json.contains(r#""label":"primary""#));
        assert!(json.contains(r#""in_cooldown":true"#));
    }

    #[test]
    fn media_guard_blocks_non_facebook_hosts() {
        assert!(media_target_allowed(
            "https://scontent.xx.fbcdn.net/v/x.jpg"
        ));
        assert!(media_target_allowed("https://video.fbcdn.net/v.mp4"));
        assert!(!media_target_allowed("https://evil.example.com/x.jpg"));
        assert!(!media_target_allowed("https://evilfbcdn.net/x.jpg"));
        assert!(!media_target_allowed("file:///etc/passwd"));
        assert!(!media_target_allowed(
            "http://169.254.169.254/latest/meta-data"
        ));
        assert!(!media_target_allowed("not a url"));
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

    #[test]
    fn oembed_json_has_author_and_provider() {
        let json = build_oembed_json("❤️ 19", "Jane Doe", "https://www.facebook.com/x", "video");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["author_name"], "❤️ 19");
        assert_eq!(value["title"], "Jane Doe");
        assert_eq!(value["provider_name"], "facebed on Rust");
        assert_eq!(value["type"], "video");
    }

    #[test]
    fn oembed_json_defaults_unknown_type_to_link() {
        let json = build_oembed_json("❤️ 1", "A", "https://x", "garbage");
        assert!(json.contains(r#""type":"link""#));
    }
}
