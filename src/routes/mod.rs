//! HTTP surface: the router, the catch-all embed handler, scrape-and-render,
//! and error responses. Path logic lives in `dispatch`, share/Activity
//! endpoints in `activity`, account racing in `race`.

use crate::config::Config;
use crate::crawler;
use crate::embed::{
    format_error_embed, format_full_post_embed, format_oversized_video_embed, format_redirect_page,
    format_reel_post_embed, format_timeout_embed,
};
use crate::error::FacebedError;
use crate::fetch::{resolve_share_link, Fetcher};
use crate::notifier::Notifier;
use crate::parsers::{
    comment::CommentParser, json_post::JsonPostParser, photocom::PhotocomParser,
    reels::ReelsParser, resolve_facebook_author_handle, single_photo::SinglePhotoParser,
    stories::StoriesParser, video_watch::VideoWatchParser, ParsedPost, Parser, ParserCtx,
};
use crate::url_clean;
use activity::{activity_status, share_activity_response, user_activity_status};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use dispatch::{is_share_path, select_kind, strip_comment_id, ParserKind};
use race::scrape_with_accounts;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tracing::{debug, error, warn};
use url::Url;

mod activity;
mod dispatch;
mod race;

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
    pub pending_activity:
        Arc<std::sync::Mutex<HashMap<String, tokio::sync::watch::Receiver<StatusCode>>>>,
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
        .route("/api/v1/statuses/:id", get(activity_status))
        .route("/users/:username/statuses/:id", get(user_activity_status))
        .route("/*path", get(catch_all))
        .with_state(state)
}

// Assets are compiled in, so the binary needs no files beside it.
static INDEX_HTML: LazyLock<String> = LazyLock::new(|| {
    include_str!("../../assets/index.html").replace("{|CREDIT|}", crate::embed::credit())
});

async fn root() -> Response {
    html_response(INDEX_HTML.clone())
}

async fn favicon() -> Response {
    static_asset(include_bytes!("../../assets/favicon.ico"), "image/x-icon")
}

async fn banner() -> Response {
    static_asset(include_bytes!("../../assets/banner.png"), "image/png")
}

fn static_asset(bytes: &'static [u8], content_type: &'static str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    (StatusCode::OK, headers, bytes).into_response()
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
    match build_oembed_json(&p.author, &p.title, &p.url, &p.kind) {
        Some(body) => json_response(body),
        None => (StatusCode::BAD_REQUEST, "url must be a Facebook post").into_response(),
    }
}

/// Public liveness probe. Reports counts only: account labels and which
/// cookie is cooling down are operator details, visible in the logs.
async fn healthz(State(state): State<AppState>) -> Response {
    use std::sync::atomic::Ordering::Relaxed;

    let jar = state.ctx.cookies.load();
    let cooling = (0..jar.len()).filter(|&i| jar.in_cooldown(i)).count();
    let body = build_healthz_json(
        state.started_at.elapsed().as_secs(),
        state.metrics.requests.load(Relaxed),
        state.metrics.errors.load(Relaxed),
        jar.len(),
        cooling,
    );
    json_response(body)
}

/// Build the oEmbed 1.0 document Discord reads to render the author/provider
/// line. Kept pure (no extractors) so it is unit-testable.
/// The embeds only ever link a Facebook post here, so any other `url` is
/// someone minting facebed-branded oEmbed documents; refuse it. Text is
/// capped for the same reason (real labels are a name or a reaction line).
fn build_oembed_json(engagement: &str, title: &str, url: &str, kind: &str) -> Option<String> {
    const MAX_CHARS: usize = 300;
    if !url_clean::is_facebook_page_url(url) {
        return None;
    }
    let cap = |s: &str| s.chars().take(MAX_CHARS).collect::<String>();
    let (engagement, title) = (cap(engagement), cap(title));
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
    .into()
}

fn build_healthz_json(
    uptime_secs: u64,
    requests: u64,
    errors: u64,
    accounts: usize,
    accounts_in_cooldown: usize,
) -> String {
    serde_json::json!({
        "status": "ok",
        "uptime_secs": uptime_secs,
        "requests": requests,
        "errors": errors,
        "cookie_accounts": accounts,
        "accounts_in_cooldown": accounts_in_cooldown,
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
    debug!(path = %path, bot = is_bot, ua = %ua, "request");
    let started = Instant::now();

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
    let is_share = is_share_path(&working);
    if is_share && ua.to_ascii_lowercase().contains("discordbot") {
        if let Some(origin) = activity_origin.as_deref() {
            return share_activity_response(&state, &working, origin).await;
        }
    }
    if is_share {
        let share_path = url_clean::clean_path(&working);
        let cached = state
            .embed_cache
            .lock()
            .ok()
            .and_then(|mut cache| cache.get_share(&share_path, Instant::now()));
        if let Some(resolved) = cached {
            working = resolved;
        } else {
            // The same cap covers resolution and scraping. Release this permit
            // before process acquires its own; never hold two for one request.
            let _permit = match state.fetch_limit.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => return busy_response(),
            };
            let remaining = match DISCORD_RESPONSE_BUDGET.checked_sub(started.elapsed()) {
                Some(remaining) => remaining,
                None => {
                    warn!(
                        path = %working,
                        elapsed_ms = started.elapsed().as_millis(),
                        "share resolve skipped after Discord response budget"
                    );
                    return no_store_html_response(format_timeout_embed(
                        &url_clean::ensure_absolute(&working),
                    ));
                }
            };
            match tokio::time::timeout(remaining, resolve_share_link(&state.fetcher, &working))
                .await
            {
                Err(_) => {
                    warn!(
                        path = %working,
                        budget_ms = DISCORD_RESPONSE_BUDGET.as_millis(),
                        "share resolve exceeded Discord response budget"
                    );
                    return no_store_html_response(format_timeout_embed(
                        &url_clean::ensure_absolute(&working),
                    ));
                }
                Ok(Ok(resolved)) if !resolved.path.is_empty() => {
                    working = resolved.path;
                    if let Ok(mut cache) = state.embed_cache.lock() {
                        cache.insert_share(share_path, working.clone(), Instant::now());
                    }
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
    }

    // strip tracking AFTER share resolve
    let (working, kind) = dispatch::route(&url_clean::clean_path(&working));
    let Some(kind) = kind else {
        return html_response(format_error_embed(
            &url_clean::ensure_absolute(&working),
            "C",
        ));
    };

    debug!(working = %working, kind = ?kind, "dispatch");
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
                debug!(path = %path, cached = true, "embed cache hit");
                return html_response(body);
            }
        }
    }

    let _permit = match state.fetch_limit.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return busy_response(),
    };

    let process_started = crate::fetch::RESPONSE_DEADLINE
        .try_with(|deadline| *deadline - DISCORD_RESPONSE_BUDGET)
        .unwrap_or_else(|_| Instant::now());
    let post = match scrape_with_accounts(state, path, kind).await {
        Ok(post) => post,
        Err(e) => return error_response(state, path, e),
    };
    let render_started = Instant::now();
    let body = render_with_size_check(state, &post, request, process_started.elapsed()).await;
    if let Ok(mut cache) = state.embed_cache.lock() {
        if activity_eligible(&post) {
            if let Some(id) = crate::activity::status_id(&post.url) {
                cache.insert_activity(&id, post, Instant::now());
            }
        }
        cache.insert(&cache_key, body.clone(), Instant::now());
    }
    debug!(
        path = %path,
        kind = ?kind,
        render_ms = render_started.elapsed().as_millis(),
        total_ms = process_started.elapsed().as_millis(),
        "embed rendered"
    );
    html_response(body)
}

// Discord's embed crawler aborts ~10.0s after fetch start (measured 2026-07-16
// by bisecting delayed-OG responses in a live channel: <=9.4s rendered every
// time, 9.5-9.6s was flaky, >=9.7s never rendered). 8500ms keeps the whole
// response inside the reliable zone with margin for edge/origin latency.
const DISCORD_RESPONSE_BUDGET: Duration = Duration::from_millis(8500);

/// Skip the video Content-Length probe once the request is this old. The
/// probe is a serial ~0.6s HEAD whose only effect is downgrading >25 MB
/// videos to a thumbnail embed; past this point the remaining crawler
/// budget matters more than that fallback.
const VIDEO_PROBE_SKIP_AFTER: Duration = Duration::from_millis(6000);

async fn process_with_deadline(
    state: &AppState,
    request: PostRequest<'_>,
    started: Instant,
) -> Response {
    let path = request.path;
    let elapsed = started.elapsed();
    let full_scrape = crate::fetch::RESPONSE_DEADLINE
        .scope(started + DISCORD_RESPONSE_BUDGET, process(state, request));
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
                    debug!(
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

/// Discord's media proxy refuses to inline videos larger than ~25 MB, leaving
/// the user with an empty player. We HEAD the first video URL and, if FB
/// advertises a content length over this limit, fall back to a thumbnail +
/// caption + link embed instead of an `og:video`.
const DISCORD_VIDEO_BYTE_LIMIT: u64 = 25 * 1024 * 1024;

fn activity_eligible(post: &ParsedPost) -> bool {
    crate::activity::eligible(post)
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
    elapsed: Duration,
) -> String {
    let tz = state.config.load().timezone;
    let Some(video_url) = post.video_links.first() else {
        return render(post, tz, request);
    };
    // Mixed image/video posts render an image grid and need no video probe.
    if !matches!(request.kind, ParserKind::Reels | ParserKind::Watch)
        && !post.image_links.is_empty()
    {
        return render(post, tz, request);
    }
    let remaining = crate::fetch::RESPONSE_DEADLINE
        .try_with(|deadline| deadline.saturating_duration_since(Instant::now()))
        .ok();
    let size = if elapsed < VIDEO_PROBE_SKIP_AFTER
        && remaining.is_none_or(|time| time > Duration::from_millis(850))
    {
        state.fetcher.head_content_length(video_url).await
    } else {
        debug!(
            url = %post.url,
            elapsed_ms = elapsed.as_millis(),
            "skipping video size probe; rendering inline video embed"
        );
        None
    };
    let Some(size) = size else {
        // Server didn't advertise Content-Length — assume it's fine and let
        // Discord try. Better to attempt the inline than silently downgrade
        // every video where FB omits the header.
        debug!(
            url = %post.url,
            "video size unavailable; rendering inline video embed"
        );
        return render(post, tz, request);
    };
    if size <= DISCORD_VIDEO_BYTE_LIMIT {
        return render(post, tz, request);
    }
    debug!(
        url = %post.url,
        bytes = size,
        limit = DISCORD_VIDEO_BYTE_LIMIT,
        "video oversized for Discord media proxy — falling back to thumbnail embed"
    );
    let activity_origin = request.activity_origin.filter(|_| activity_eligible(post));
    format_oversized_video_embed(post, tz, activity_origin)
}

fn error_response(state: &AppState, path: &str, e: FacebedError) -> Response {
    state
        .metrics
        .errors
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let url = url_clean::ensure_absolute(path);
    let code = e.error_code();
    match &e {
        FacebedError::NoData(msg) | FacebedError::LoginWall(msg) => {
            debug!(path = %path, "no data: {}", msg);
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
pub(crate) mod test_support {
    use super::{AppState, Metrics};
    use std::sync::Arc;
    use std::time::Instant;

    pub(crate) fn test_state() -> AppState {
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
            pending_activity: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            metrics: Arc::new(Metrics::default()),
            started_at: Instant::now(),
        }
    }

    pub(crate) fn activity_post() -> crate::parsers::ParsedPost {
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
}

#[cfg(test)]
mod tests {
    use super::test_support::{activity_post, test_state};
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request, StatusCode};

    use std::time::Instant;
    use tower::ServiceExt;
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

    #[tokio::test]
    async fn humans_are_redirected_even_for_type_3_photo_links() {
        let response = router(test_state())
            .oneshot(
                Request::builder()
                    .uri("/photo.php?fbid=1&set=a.2&type=3")
                    .header(header::USER_AGENT, "Mozilla/5.0 (iPhone)")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    }

    #[test]
    fn healthz_json_reports_counters_and_accounts() {
        let json = build_healthz_json(42, 7, 1, 2, 1);
        assert!(json.contains(r#""status":"ok""#));
        assert!(json.contains(r#""uptime_secs":42"#));
        assert!(json.contains(r#""requests":7"#));
        assert!(json.contains(r#""errors":1"#));
        assert!(json.contains(r#""cookie_accounts":2"#));
        assert!(json.contains(r#""accounts_in_cooldown":1"#));
        assert!(!json.contains("label"));
    }

    #[test]
    fn oembed_json_has_author_and_provider() {
        let json =
            build_oembed_json("❤️ 19", "Jane Doe", "https://www.facebook.com/x", "video").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["author_name"], "❤️ 19");
        assert_eq!(value["title"], "Jane Doe");
        assert_eq!(value["provider_name"], "facebed on Rust");
        assert_eq!(value["type"], "video");
    }

    #[test]
    fn oembed_json_defaults_unknown_type_to_link() {
        let json = build_oembed_json("❤️ 1", "A", "https://www.facebook.com/x", "garbage").unwrap();
        assert!(json.contains(r#""type":"link""#));
    }

    #[test]
    fn oembed_refuses_non_facebook_urls_and_caps_text() {
        assert!(build_oembed_json("a", "b", "https://evil.example/x", "link").is_none());
        let long = "x".repeat(5000);
        let json = build_oembed_json(&long, &long, "https://www.facebook.com/x", "link").unwrap();
        assert!(json.len() < 1000);
    }
}
