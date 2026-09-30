//! Mastodon-style Activity endpoints Discord reads for share links, and the
//! share-discovery scrape behind them.

use super::dispatch::{is_share_path, ParserKind};
use super::race::scrape_with_accounts;
use super::{
    json_response, json_status_response, no_store_html_response, render, request_origin, AppState,
    PostRequest, DISCORD_RESPONSE_BUDGET,
};
use crate::error::FacebedError;
use crate::fetch::resolve_share_link;
use crate::parsers::ParsedPost;
use crate::url_clean;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use std::time::Instant;
use tracing::{debug, warn};

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

pub(super) async fn activity_status(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: HeaderMap,
) -> Response {
    activity_status_for_id(state, id, request_origin(&headers).unwrap_or_default()).await
}

pub(super) async fn user_activity_status(
    State(state): State<AppState>,
    axum::extract::Path((_username, id)): axum::extract::Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    activity_status_for_id(state, id, request_origin(&headers).unwrap_or_default()).await
}

async fn activity_status_for_id(state: AppState, id: String, origin: String) -> Response {
    match activity_post_for_id(&state, &id).await {
        Ok(post) => json_response(crate::activity::status_json(&id, &post, &origin)),
        Err(status) => activity_error_response(status),
    }
}

async fn activity_post_for_id(state: &AppState, id: &str) -> Result<ParsedPost, StatusCode> {
    let mut completion = start_activity(state, id)?;
    // watch retains completion even if the scrape finishes before this request
    // begins waiting. Notify::notify_waiters would lose that wakeup.
    if *completion.borrow() == StatusCode::SERVICE_UNAVAILABLE
        && tokio::time::timeout(DISCORD_RESPONSE_BUDGET, completion.changed())
            .await
            .is_err()
    {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let status = *completion.borrow();
    if status == StatusCode::OK {
        if let Ok(mut cache) = state.embed_cache.lock() {
            if let Some(post) = cache.get_activity(id, Instant::now()) {
                return Ok(post);
            }
        }
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    Err(status)
}

/// Share discovery and Activity hydration join the same bounded scrape.
fn start_activity(
    state: &AppState,
    id: &str,
) -> Result<tokio::sync::watch::Receiver<StatusCode>, StatusCode> {
    let path = crate::activity::decode_status_path(id).ok_or(StatusCode::BAD_REQUEST)?;
    let kind = if is_share_path(&path) {
        None
    } else {
        Some(activity_path(id)?)
    };
    let mut pending = state
        .pending_activity
        .lock()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    // Check under the pending lock so a completing scrape cannot slip between
    // the cache lookup and registering its replacement.
    if let Ok(mut cache) = state.embed_cache.lock() {
        if cache.get_activity(id, Instant::now()).is_some() {
            let (_, completion) = tokio::sync::watch::channel(StatusCode::OK);
            return Ok(completion);
        }
    }
    if let Some(completion) = pending.get(id) {
        return Ok(completion.clone());
    }
    let permit = state
        .fetch_limit
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let (finished, completion) = tokio::sync::watch::channel(StatusCode::SERVICE_UNAVAILABLE);
    pending.insert(id.to_owned(), completion.clone());
    let state = state.clone();
    let id = id.to_owned();
    tokio::spawn(async move {
        let _permit = permit;
        let started = Instant::now();
        let result = tokio::time::timeout(
            DISCORD_RESPONSE_BUDGET,
            crate::fetch::RESPONSE_DEADLINE.scope(started + DISCORD_RESPONSE_BUDGET, async {
                match kind {
                    None => scrape_share_with_accounts(&state, &path).await,
                    Some((path, kind)) => scrape_with_accounts(&state, &path, kind).await,
                }
            }),
        )
        .await;
        let status = match result {
            Ok(Ok(post)) => {
                if let Ok(mut cache) = state.embed_cache.lock() {
                    if let Some(canonical_id) = crate::activity::status_id(&post.url) {
                        cache.insert_activity(&canonical_id, post.clone(), Instant::now());
                    }
                    cache.insert_activity(&id, post, Instant::now());
                }
                StatusCode::OK
            }
            Ok(Err(_)) => StatusCode::NOT_FOUND,
            Err(_) => {
                warn!(path = %path, budget_ms = DISCORD_RESPONSE_BUDGET.as_millis(), "activity scrape exceeded Discord response budget");
                StatusCode::SERVICE_UNAVAILABLE
            }
        };
        debug!(path = %path, elapsed_ms = started.elapsed().as_millis(), %status, "activity scrape finished");
        let _ = finished.send(status);
        if let Ok(mut pending) = state.pending_activity.lock() {
            pending.remove(&id);
        }
    });
    Ok(completion)
}

pub(super) async fn share_activity_response(
    state: &AppState,
    path: &str,
    origin: &str,
) -> Response {
    let post_url = url_clean::ensure_absolute(&url_clean::clean_path(path));
    let Some(id) = crate::activity::status_id(&post_url) else {
        return activity_error_response(StatusCode::BAD_REQUEST);
    };
    let post = match activity_post_for_id(state, &id).await {
        Ok(post) => post,
        Err(status) => return activity_error_response(status),
    };
    no_store_html_response(render(
        &post,
        state.config.load().timezone,
        PostRequest {
            path,
            kind: ParserKind::JsonPost,
            activity_origin: Some(origin),
        },
    ))
}

async fn scrape_share_with_accounts(
    state: &AppState,
    path: &str,
) -> Result<ParsedPost, FacebedError> {
    let share_path = url_clean::clean_path(path);
    let cached = state
        .embed_cache
        .lock()
        .ok()
        .and_then(|mut cache| cache.get_share(&share_path, Instant::now()));
    let resolved = match cached {
        Some(resolved) => resolved,
        None => {
            let resolved = resolve_share_link(&state.fetcher, path).await?.path;
            if let Ok(mut cache) = state.embed_cache.lock() {
                cache.insert_share(share_path, resolved.clone(), Instant::now());
            }
            resolved
        }
    };
    let id = crate::activity::status_id(&url_clean::ensure_absolute(&resolved))
        .ok_or_else(|| FacebedError::no_data("share did not resolve"))?;
    let (path, kind) =
        activity_path(&id).map_err(|_| FacebedError::no_data("unsupported share target"))?;
    scrape_with_accounts(state, &path, kind).await
}

fn activity_path(id: &str) -> Result<(String, ParserKind), StatusCode> {
    let path = crate::activity::decode_status_path(id).ok_or(StatusCode::BAD_REQUEST)?;
    match super::dispatch::route(&path) {
        (path, Some(kind)) => Ok((path, kind)),
        (_, None) => Err(StatusCode::NOT_FOUND),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::router;
    use crate::routes::test_support::{activity_post, test_state};
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request, StatusCode};

    use std::time::Instant;
    use tower::ServiceExt;
    #[tokio::test]
    async fn share_activity_joins_one_scrape_without_queueing() {
        let state = test_state();
        state.fetch_limit.add_permits(1);
        let path = "share/v/example/";
        let id = crate::activity::status_id(&crate::url_clean::ensure_absolute(path)).unwrap();
        let first = super::start_activity(&state, &id).expect("cold scrape");
        assert_eq!(state.fetch_limit.available_permits(), 0);
        let second = super::start_activity(&state, &id).expect("shared scrape");
        assert!(first.same_channel(&second));
        assert_eq!(state.pending_activity.lock().unwrap().len(), 1);
        let second_id =
            crate::activity::status_id("https://www.facebook.com/share/v/other/").unwrap();
        assert!(matches!(
            super::start_activity(&state, &second_id),
            Err(StatusCode::SERVICE_UNAVAILABLE)
        ));
        // No await: the spawned fetch has not been polled. The runtime cancels
        // it on test completion, so this exercises admission without Facebook.
    }

    #[tokio::test]
    async fn share_discovery_resolves_author_label_and_activity_from_same_post() {
        for handle in [Some("example.author"), None] {
            let state = test_state();
            let path = "share/v/example/";
            let id = crate::activity::status_id(&crate::url_clean::ensure_absolute(path)).unwrap();
            let mut post = activity_post();
            post.author_handle = handle.map(str::to_owned);
            post.author_id = Some("61579685171950".into());
            post.author_avatar_url = Some("https://img.example/uploader.jpg".into());
            state
                .embed_cache
                .lock()
                .unwrap()
                .insert_activity(&id, post.clone(), Instant::now());
            let response =
                super::share_activity_response(&state, path, "https://facebed.example").await;
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body = String::from_utf8(body.to_vec()).unwrap();
            let handle = handle.unwrap_or(post.author_id.as_deref().unwrap());
            assert!(body.contains("type=\"application/json+oembed\""));
            assert!(body.contains("type=\"application/activity+json\""));
            assert!(body.contains(&format!("users/{handle}/statuses/")));
            assert!(body.contains(&format!("{} (@{handle})", post.author_name)));
            assert!(body.contains("/oembed.json?author="));
        }
    }

    #[tokio::test]
    async fn share_activity_waits_for_completion_and_retains_early_failures() {
        use std::future::{poll_fn, Future};
        use std::task::Poll;

        let state = test_state();
        let id = crate::activity::status_id("https://www.facebook.com/share/v/example/").unwrap();
        let (finished, completion) = tokio::sync::watch::channel(StatusCode::SERVICE_UNAVAILABLE);
        state
            .pending_activity
            .lock()
            .unwrap()
            .insert(id.clone(), completion);
        let mut response = Box::pin(super::activity_status_for_id(
            state.clone(),
            id.clone(),
            String::new(),
        ));
        assert!(poll_fn(|cx| Poll::Ready(response.as_mut().poll(cx).is_pending())).await);
        let post = activity_post();
        let expected = crate::activity::status_json(&id, &post, "");
        state
            .embed_cache
            .lock()
            .unwrap()
            .insert_activity(&id, post, Instant::now());
        finished.send(StatusCode::OK).unwrap();
        drop(finished);
        let response = tokio::time::timeout(std::time::Duration::from_millis(100), response)
            .await
            .expect("completed scrape must wake Activity hydration");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .as_ref(),
            expected.as_bytes()
        );

        let failed_id =
            crate::activity::status_id("https://www.facebook.com/share/v/missing/").unwrap();
        let (finished, completion) = tokio::sync::watch::channel(StatusCode::SERVICE_UNAVAILABLE);
        finished.send(StatusCode::NOT_FOUND).unwrap();
        drop(finished);
        state
            .pending_activity
            .lock()
            .unwrap()
            .insert(failed_id.clone(), completion);
        let response = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            super::activity_status_for_id(state, failed_id, String::new()),
        )
        .await
        .expect("early completion is retained");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn activity_alias_returns_cached_status_json_when_preloaded() {
        // Given
        let state = test_state();
        let post = activity_post();
        let id = crate::activity::status_id(&post.url).expect("activity status id");
        let expected = crate::activity::status_json(&id, &post, "");
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
    async fn activity_alias_keeps_clickable_facebook_post_on_preview_origin() {
        let state = test_state();
        let post = activity_post();
        let id = crate::activity::status_id(&post.url).unwrap();
        state
            .embed_cache
            .lock()
            .unwrap()
            .insert_activity(&id, post.clone(), Instant::now());
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri(format!("/users/example.author/statuses/{id}"))
                    .header(header::HOST, "preview.example")
                    .header("x-forwarded-proto", "https")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["account"]["url"], post.url);
    }

    #[tokio::test]
    async fn activity_alias_returns_cached_video_status_json_when_preloaded() {
        // Given
        let state = test_state();
        let mut post = activity_post();
        post.video_links = vec!["https://video.example/post.mp4".into()];
        post.thumbnail = Some("https://img.example/post.jpg".into());
        let id = crate::activity::status_id(&post.url).expect("activity status id");
        let expected = crate::activity::status_json(&id, &post, "");
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
}
