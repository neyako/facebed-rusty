use crate::config::Config;
use crate::crawler;
use crate::embed::{
    format_error_embed, format_full_post_embed, format_oversized_video_embed,
    format_redirect_page, format_reel_post_embed,
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
            headers.insert(axum::http::header::CONTENT_TYPE, HeaderValue::from_static(ct));
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
static RE_SHARE_PR: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(/)?share/([pr]/)?[a-zA-Z0-9\-._]*(/)?").unwrap());
static RE_STORIES: Lazy<Regex> = Lazy::new(|| Regex::new(r"^/?stories/\d+/[A-Za-z0-9=_-]+").unwrap());

fn is_facebook_url(path: &str) -> bool {
    let username_pat = r"[a-zA-Z0-9\-._]*";
    let full = format!("https://www.facebook.com/{path}");
    let Ok(parsed) = Url::parse(&full) else { return false };
    let p = parsed.path();
    let is_group = Regex::new(&format!("^/groups/{username_pat}")).unwrap().is_match(p);
    let is_permalink = p.starts_with("/permalink.php");
    let is_story = p.starts_with("/story.php");
    let is_post = Regex::new(&format!("/{username_pat}/posts")).unwrap().is_match(p);
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
        hdrs.insert(axum::http::header::LOCATION, HeaderValue::from_str(&target).unwrap_or_else(|_| HeaderValue::from_static("/")));
        hdrs.insert(axum::http::header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
        return (StatusCode::MOVED_PERMANENTLY, hdrs, body).into_response();
    }

    // share link resolve
    let mut working = path.clone();
    if RE_SHARE_V.is_match(&working) || RE_SHARE_PR.is_match(&working) {
        match resolve_share_link(&state.fetcher, &working).await {
            Ok(p) if !p.is_empty() => working = p,
            Ok(_) => {
                return html_response(format_error_embed(&url_clean::ensure_absolute(&working), "C"));
            }
            Err(e) => return error_response(&state, &working, e),
        }
    }

    // strip tracking AFTER share resolve
    working = url_clean::clean_path(&working);

    // /videos/<id> → reel/<id>
    if let Some(caps) = RE_VIDEOS.captures(&working) {
        working = format!("reel/{}", &caps[1]);
    }

    // dispatch
    let kind = if RE_STORIES.is_match(&working) {
        ParserKind::Stories
    } else if RE_REEL.is_match(&working) {
        ParserKind::Reels
    } else if path_only(&working).map(|p| RE_PHOTO.is_match(&p)).unwrap_or(false) {
        ParserKind::SinglePhoto
    } else if path_only(&working).map(|p| RE_WATCH.is_match(&p)).unwrap_or(false) {
        ParserKind::Watch
    } else if RE_PAGE_VIDEO.is_match(&working) {
        ParserKind::Watch
    } else if is_facebook_url(&working) {
        ParserKind::JsonPost
    } else {
        return html_response(format_error_embed("https://git.facebed.com", "C"));
    };

    info!(working = %working, kind = ?kind, "dispatch");
    process(&state, &working, kind).await
}

fn path_only(s: &str) -> Option<String> {
    Url::parse(&format!("https://www.facebook.com/{}", s.trim_start_matches('/')))
        .ok()
        .map(|u| u.path().to_owned())
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
    // Retry across every cookie account. With multi-account setups, a given post
    // may only be visible to some accounts — round-robin happily picks ones that
    // can't view it. We loop deterministically through all accounts (seeded from
    // the current round-robin cursor so cold requests still rotate fairly) and
    // bail to error_response only after every account has failed.
    //
    // Cooldown: accounts that failed recently are skipped on the first pass so
    // we don't pay a slow FB round-trip on a checkpointed/expired account
    // every other request. They're still tried as a last resort if no healthy
    // account succeeded.
    let n = state.ctx.cookies.len();
    let attempts = n.max(1);
    let start = state.ctx.cookies.cursor();
    let mut last_err: Option<FacebedError> = None;
    let advance_cursor = n > 0;

    // Build ordering: healthy accounts first (in round-robin order), then
    // cooldowned ones as fallback. With n=0 (anonymous) we still loop once.
    let order: Vec<usize> = if n > 0 {
        let mut healthy = Vec::new();
        let mut cooled = Vec::new();
        for attempt in 0..attempts {
            let i = start.wrapping_add(attempt) % n;
            if state.ctx.cookies.in_cooldown(i) {
                cooled.push(i);
            } else {
                healthy.push(i);
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
                if advance_cursor {
                    state.ctx.cookies.mark_ok(account_index);
                    state.ctx.cookies.advance_cursor();
                }
                let body = render_with_size_check(state, &post, kind).await;
                return html_response(body);
            }
            Err(e) if is_retryable(&e) && loop_idx + 1 < order.len() => {
                if advance_cursor {
                    state.ctx.cookies.mark_failed(account_index);
                }
                let label = state.ctx.cookies.label_at(account_index).unwrap_or("?");
                warn!(path = %path, attempt = loop_idx, account = %label, error = %e, "retrying with next account");
                last_err = Some(e);
                continue;
            }
            Err(e) => {
                if advance_cursor {
                    state.ctx.cookies.mark_failed(account_index);
                    state.ctx.cookies.advance_cursor();
                }
                return error_response(state, path, e);
            }
        }
    }

    if advance_cursor {
        state.ctx.cookies.advance_cursor();
    }
    error_response(
        state,
        path,
        last_err.unwrap_or_else(|| FacebedError::no_data(String::from("no accounts available"))),
    )
}

async fn run_parser(state: &AppState, path: &str, kind: ParserKind) -> Result<ParsedPost, FacebedError> {
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
    matches!(e, FacebedError::NoData(_) | FacebedError::Parse { .. })
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

fn error_response(state: &AppState, path: &str, e: FacebedError) -> Response {
    let url = url_clean::ensure_absolute(path);
    let code = e.error_code();
    match &e {
        FacebedError::NoData(msg) => {
            info!(path = %path, "no data: {}", msg);
        }
        FacebedError::Parse { message, html, url: u } => {
            error!(path = %path, error = %message, "parser bug");
            let page_url = u.clone().unwrap_or_else(|| url.clone());
            let warn_msg = format!("🚨 **ParseException** for `{path}`\n{page_url}\n`{message}`");
            if let Some(h) = html {
                let safe = path
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                    .take(80)
                    .collect::<String>();
                state.notifier.warn(warn_msg, Some((format!("{safe}.html"), h.clone().into_bytes())));
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
