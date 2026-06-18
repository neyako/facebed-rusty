use crate::cookies::CookieJar;
use crate::error::{FacebedError, FacebedResult};
use crate::jq;
use crate::url_clean::{ensure_absolute, is_facebook_media_host, is_facebook_page_host};
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::{Client, RequestBuilder};
use scraper::{Html, Selector};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::Url;

tokio::task_local! {
    /// When set, [`Fetcher::fetch`] uses this account index (modulo account count)
    /// instead of the primary account. Used by the per-request retry loop to
    /// deterministically try fallback cookie accounts.
    pub static ACCOUNT_OVERRIDE: usize;
}

pub struct Fetcher {
    client: Client,
    cookies: Arc<arc_swap::ArcSwap<CookieJar>>,
    media_size_cache: Mutex<MediaSizeCache>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageType {
    HasData,
    LoginWall,
    Unknown,
}

pub struct FetchedPage {
    pub url: String,
    pub html: String,
    document: Html,
    partial: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedShare {
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct CookieAccountCheck {
    pub index: usize,
    pub label: String,
    pub ok: bool,
    pub account_name: Option<String>,
    pub status: Option<u16>,
    pub reason: Option<String>,
}

impl FetchedPage {
    pub fn document(&self) -> &Html {
        &self.document
    }

    pub fn is_partial(&self) -> bool {
        self.partial
    }
}

/// Fallback UA when an account has no `user_agent` set in its cookie file
/// and for anonymous (no-cookie) requests.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.36";

const HEADERS: &[(&str, &str)] = &[
    ("accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/jxl,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7"),
    ("accept-language", "en-US,en;q=0.9"),
    ("cache-control", "no-cache"),
    ("pragma", "no-cache"),
    ("priority", "u=0, i"),
    ("sec-fetch-mode", "navigate"),
    ("sec-fetch-site", "none"),
];
const SHARE_HEAD_USER_AGENT: &str = "python-requests/2.32.3";
const VIDEO_HEAD_TIMEOUT: Duration = Duration::from_millis(750);
const VIDEO_HEAD_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const VIDEO_HEAD_CACHE_MAX: usize = 256;

#[derive(Default)]
struct MediaSizeCache {
    entries: HashMap<String, CachedContentLength>,
}

#[derive(Clone, Copy)]
struct CachedContentLength {
    value: Option<u64>,
    checked_at: Instant,
}

impl MediaSizeCache {
    fn get(&mut self, url: &str, now: Instant) -> Option<Option<u64>> {
        let Some(entry) = self.entries.get(url).copied() else {
            return None;
        };
        if now.duration_since(entry.checked_at) <= VIDEO_HEAD_CACHE_TTL {
            return Some(entry.value);
        }
        self.entries.remove(url);
        None
    }

    fn insert(&mut self, url: &str, value: Option<u64>, now: Instant) {
        if self.entries.len() >= VIDEO_HEAD_CACHE_MAX && !self.entries.contains_key(url) {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.checked_at)
                .map(|(url, _)| url.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            url.to_owned(),
            CachedContentLength {
                value,
                checked_at: now,
            },
        );
    }
}

impl Fetcher {
    pub fn new(cookies: Arc<arc_swap::ArcSwap<CookieJar>>) -> anyhow::Result<Self> {
        let client = Client::builder()
            .gzip(true)
            .brotli(true)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(8))
            .build()?;
        Ok(Self {
            client,
            cookies,
            media_size_cache: Mutex::default(),
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub async fn check_cookie_accounts(&self) -> Vec<CookieAccountCheck> {
        let n = self.cookies.load().len();
        let mut checks = Vec::with_capacity(n);
        for i in 0..n {
            checks.push(self.check_cookie_account(i).await);
        }
        checks
    }

    pub async fn check_cookie_account(&self, account_index: usize) -> CookieAccountCheck {
        let guard = self.cookies.load();
        let Some(acc) = guard.account_at(account_index) else {
            return CookieAccountCheck {
                index: account_index,
                label: format!("#{account_index}"),
                ok: false,
                account_name: None,
                status: None,
                reason: Some("account missing".into()),
            };
        };
        let label = acc.label.clone();
        let cookie = acc.header_value();
        let user_agent = acc
            .user_agent
            .clone()
            .unwrap_or_else(|| DEFAULT_USER_AGENT.to_owned());

        let mut req = self.client.get("https://www.facebook.com/me");
        for (k, v) in HEADERS {
            req = req.header(*k, *v);
        }
        req = req
            .header("cookie", cookie)
            .header("user-agent", user_agent);

        let resp = match req.send().await {
            Ok(resp) => resp,
            Err(e) => {
                return CookieAccountCheck {
                    index: account_index,
                    label,
                    ok: false,
                    account_name: None,
                    status: e.status().map(|s| s.as_u16()),
                    reason: Some(format!("http: {e}")),
                };
            }
        };
        let status = resp.status();
        let final_url = resp.url().to_string();
        let body = match resp.text().await {
            Ok(body) => body,
            Err(e) => {
                return CookieAccountCheck {
                    index: account_index,
                    label,
                    ok: false,
                    account_name: None,
                    status: Some(status.as_u16()),
                    reason: Some(format!("read body: {e}")),
                };
            }
        };

        if !status.is_success() {
            return CookieAccountCheck {
                index: account_index,
                label,
                ok: false,
                account_name: None,
                status: Some(status.as_u16()),
                reason: Some(format!("status {status}")),
            };
        }

        let doc = Html::parse_document(&body);
        if let Some(reason) = cookie_probe_blocked_reason(&final_url, &body, &doc) {
            return CookieAccountCheck {
                index: account_index,
                label,
                ok: false,
                account_name: None,
                status: Some(status.as_u16()),
                reason: Some(reason.into()),
            };
        }

        let account_name = extract_account_name(&doc, &body);
        let ok = account_name.is_some();
        CookieAccountCheck {
            index: account_index,
            label,
            ok,
            account_name,
            status: Some(status.as_u16()),
            reason: if ok {
                None
            } else {
                Some("account name not found".into())
            },
        }
    }

    /// HEAD the URL and return `Content-Length` if the server advertises one.
    /// Used to gate Discord embed size — Discord's media proxy refuses to
    /// inline videos past ~25 MB, so we want to detect oversize before we
    /// hand the URL off as an `og:video`. Returns `None` on transport error,
    /// non-2xx response, or missing/unparseable header.
    pub async fn head_content_length(&self, url: &str) -> Option<u64> {
        let host_ok = Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(is_facebook_media_host))
            .unwrap_or(false);
        if !host_ok {
            return None;
        }
        let started = Instant::now();
        let now = Instant::now();
        if let Ok(mut cache) = self.media_size_cache.lock() {
            if let Some(value) = cache.get(url, now) {
                tracing::info!(
                    size = ?value,
                    cached = true,
                    elapsed_ms = started.elapsed().as_millis(),
                    "video size probe done"
                );
                return value;
            }
        }

        let resp = match self
            .client
            .head(url)
            .timeout(VIDEO_HEAD_TIMEOUT)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                let e = e.without_url();
                tracing::debug!(
                    error = %e,
                    elapsed_ms = started.elapsed().as_millis(),
                    "video size probe failed"
                );
                return None;
            }
        };
        let status = resp.status();
        if !resp.status().is_success() {
            if let Ok(mut cache) = self.media_size_cache.lock() {
                cache.insert(url, None, Instant::now());
            }
            tracing::debug!(
                status = %status,
                elapsed_ms = started.elapsed().as_millis(),
                "video size probe rejected status"
            );
            return None;
        }
        let value = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());
        if let Ok(mut cache) = self.media_size_cache.lock() {
            cache.insert(url, value, Instant::now());
        }
        tracing::info!(
            status = %status,
            size = ?value,
            cached = false,
            elapsed_ms = started.elapsed().as_millis(),
            "video size probe done"
        );
        value
    }

    /// Fetch a Facebook path. Optionally attach cookies. Raises NoData on login walls.
    pub async fn fetch(&self, post_path: &str, use_cookies: bool) -> FacebedResult<FetchedPage> {
        let started = Instant::now();
        let url = facebook_fetch_url(post_path)?;
        let (req, account_label) = self.request_for(&url, use_cookies);
        let resp = req.send().await?;
        let response_ms = started.elapsed().as_millis();
        let status = resp.status();
        let final_url = resp.url().to_string();
        let retry_after = retry_after_secs(&resp);
        if let Some(err) = classify_block(status, &final_url, retry_after) {
            tracing::warn!(path = %post_path, account = %account_label, status = %status, final_url = %final_url, response_ms, "fetch blocked");
            return Err(err);
        }
        let read_started = Instant::now();
        let html = resp.text().await?;
        let read_ms = read_started.elapsed().as_millis();
        tracing::info!(path = %post_path, account = %account_label, status = %status, final_url = %final_url, len = html.len(), partial = false, response_ms, read_ms, total_ms = started.elapsed().as_millis(), "fetch done");
        self.page_from_html(url, html, post_path, false)
    }

    /// Fetch a Facebook path, stopping early once `should_stop` says the
    /// downloaded prefix contains enough data for the caller. If it never
    /// matches, this behaves like [`fetch`].
    pub async fn fetch_until<F>(
        &self,
        post_path: &str,
        use_cookies: bool,
        mut should_stop: F,
    ) -> FacebedResult<FetchedPage>
    where
        F: FnMut(&[u8]) -> bool,
    {
        let started = Instant::now();
        let url = facebook_fetch_url(post_path)?;
        let (req, account_label) = self.request_for(&url, use_cookies);
        let mut resp = req.send().await?;
        let response_ms = started.elapsed().as_millis();
        let status = resp.status();
        let final_url = resp.url().to_string();
        let retry_after = retry_after_secs(&resp);
        if let Some(err) = classify_block(status, &final_url, retry_after) {
            tracing::warn!(path = %post_path, account = %account_label, status = %status, final_url = %final_url, response_ms, "fetch blocked");
            return Err(err);
        }
        let mut body = Vec::new();
        let mut stopped_early = false;
        let read_started = Instant::now();
        while let Some(chunk) = resp.chunk().await? {
            body.extend_from_slice(&chunk);
            if should_stop(&body) {
                stopped_early = true;
                break;
            }
        }
        let read_ms = read_started.elapsed().as_millis();
        let html = String::from_utf8_lossy(&body).into_owned();
        tracing::info!(path = %post_path, account = %account_label, status = %status, final_url = %final_url, len = html.len(), partial = stopped_early, response_ms, read_ms, total_ms = started.elapsed().as_millis(), "fetch done");
        self.page_from_html(url, html, post_path, stopped_early)
    }

    fn request_for(&self, url: &str, use_cookies: bool) -> (RequestBuilder, String) {
        let mut req = self.client.get(url);
        for (k, v) in HEADERS {
            req = req.header(*k, *v);
        }
        let mut account_label = String::new();
        let mut user_agent: &str = DEFAULT_USER_AGENT;
        let guard = self.cookies.load();
        if use_cookies {
            let acc = ACCOUNT_OVERRIDE
                .try_with(|i| guard.account_at(*i))
                .ok()
                .flatten()
                .or_else(|| guard.account_at(0));
            if let Some(acc) = acc {
                account_label = acc.label.clone();
                req = req.header("cookie", acc.header_value());
                if let Some(ua) = acc.user_agent.as_deref() {
                    user_agent = ua;
                }
            }
        }
        (req.header("user-agent", user_agent), account_label)
    }

    fn page_from_html(
        &self,
        url: String,
        html: String,
        post_path: &str,
        partial: bool,
    ) -> FacebedResult<FetchedPage> {
        let parse_started = Instant::now();
        let document = Html::parse_document(&html);
        let parse_ms = parse_started.elapsed().as_millis();
        let page = FetchedPage {
            url,
            html,
            document,
            partial,
        };
        let probe_started = Instant::now();
        check_or_raise(&page, post_path)?;
        tracing::debug!(
            path = %post_path,
            partial,
            parse_ms,
            probe_ms = probe_started.elapsed().as_millis(),
            "facebook html parsed"
        );
        Ok(page)
    }
}

/// Resolve `post_path` to an absolute URL, refusing anything that is not a
/// Facebook page host. Content fetches can attach the account cookie, so only
/// Facebook page hosts may receive these requests.
fn facebook_fetch_url(post_path: &str) -> FacebedResult<String> {
    let url = ensure_absolute(post_path);
    let allowed = Url::parse(&url)
        .ok()
        .and_then(|u| u.host_str().map(is_facebook_page_host))
        .unwrap_or(false);
    if allowed {
        Ok(url)
    } else {
        Err(FacebedError::no_data(format!(
            "refusing to fetch non-Facebook host for {post_path}"
        )))
    }
}

/// Resolve a `/share/v/...` or `/share/[pr]/...` link to its canonical
/// content path.
///
/// Cookie accounts are tried first in configured priority order. Public
/// fallback only exists for no-cookie dev deployments; prod uses the same
/// cookie path for share resolution and content fetches.
pub async fn resolve_share_link(fetcher: &Fetcher, path: &str) -> FacebedResult<ResolvedShare> {
    let is_share_v = is_share_v_path(path);
    if !fetcher.cookies.load().is_empty() {
        if let Some(resolved) = resolve_share_link_with_accounts(fetcher, path, is_share_v).await {
            return Ok(resolved);
        }
        return Ok(ResolvedShare {
            path: String::new(),
        });
    }

    resolve_share_link_public(fetcher, path, is_share_v).await
}

async fn resolve_share_link_public(
    fetcher: &Fetcher,
    path: &str,
    is_share_v: bool,
) -> FacebedResult<ResolvedShare> {
    let head_path = resolve_share_link_head(fetcher, path, None).await;
    if let Some(path) = head_path.as_deref() {
        if head_target_usable(path, is_share_v) {
            return Ok(ResolvedShare {
                path: path.to_owned(),
            });
        }
    }

    let resolved = resolve_share_link_body(fetcher, path, None).await?;
    if share_resolution_usable(&resolved) {
        return Ok(resolved);
    }

    Ok(resolved)
}

fn is_share_v_path(path: &str) -> bool {
    path.trim_start_matches('/').starts_with("share/v/")
}

fn is_post_like_share_target(path: &str) -> bool {
    let parsed = Url::parse(&ensure_absolute(path)).ok();
    let path = parsed
        .as_ref()
        .map(|u| u.path().trim_start_matches('/'))
        .unwrap_or_else(|| path.trim_start_matches('/'));
    path.starts_with("watch")
        || path.starts_with("reel/")
        || path.contains("/videos/")
        || (path.starts_with("groups/")
            && (path.contains("/permalink/") || path.contains("/posts/")))
}

fn head_target_usable(path: &str, is_share_v: bool) -> bool {
    !is_group_landing_target(path) && (!is_share_v || is_post_like_share_target(path))
}

fn share_resolution_usable(resolved: &ResolvedShare) -> bool {
    !resolved.path.is_empty() && !is_group_landing_target(&resolved.path)
}

fn is_group_landing_target(path: &str) -> bool {
    let parsed = Url::parse(&ensure_absolute(path)).ok();
    let path = parsed
        .as_ref()
        .map(|u| u.path().trim_matches('/'))
        .unwrap_or_else(|| path.trim_matches('/'));
    path.starts_with("groups/") && (path.ends_with("/about") || path.split('/').count() <= 2)
}

async fn resolve_share_link_with_accounts(
    fetcher: &Fetcher,
    path: &str,
    is_share_v: bool,
) -> Option<ResolvedShare> {
    for account_index in share_account_order(fetcher) {
        let label = fetcher
            .cookies
            .load()
            .label_at(account_index)
            .unwrap_or("?")
            .to_owned();

        if let Some(path) = resolve_share_link_head(fetcher, path, Some(account_index)).await {
            if head_target_usable(&path, is_share_v) {
                tracing::info!(account = %label, resolved = %path, "resolved share link with account head");
                return Some(ResolvedShare { path });
            }
        }

        match resolve_share_link_body(fetcher, path, Some(account_index)).await {
            Ok(resolved) if share_resolution_usable(&resolved) => {
                tracing::info!(account = %label, resolved = %resolved.path, "resolved share link with account body");
                return Some(resolved);
            }
            Ok(resolved) => {
                tracing::warn!(
                    account = %label,
                    resolved = %resolved.path,
                    "account share resolve did not produce post target"
                );
            }
            Err(e) => {
                tracing::warn!(account = %label, error = %e, "account share resolve failed");
            }
        }
    }
    None
}

fn share_account_order(fetcher: &Fetcher) -> Vec<usize> {
    let guard = fetcher.cookies.load();
    let n = guard.len();
    let mut healthy = Vec::new();
    let mut cooled = Vec::new();
    for i in 0..n {
        if guard.in_cooldown(i) {
            cooled.push(i);
        } else {
            healthy.push(i);
        }
    }
    healthy.into_iter().chain(cooled).collect()
}

async fn resolve_share_link_body(
    fetcher: &Fetcher,
    path: &str,
    account_index: Option<usize>,
) -> FacebedResult<ResolvedShare> {
    let started = Instant::now();
    let url = ensure_absolute(path);
    let mut req = fetcher.client().get(&url);
    for (k, v) in HEADERS {
        req = req.header(*k, *v);
    }
    req = attach_share_identity(
        fetcher,
        req,
        account_index,
        "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)",
    );
    let resp = req.send().await?;
    let response_ms = started.elapsed().as_millis();
    let status = resp.status();
    let final_url = resp.url().to_string();
    let read_started = Instant::now();
    let body = resp.text().await?;
    let read_ms = read_started.elapsed().as_millis();

    // Prefer og:url / link[rel=canonical] from the response body — that
    // value is the page's own declared canonical path, not just wherever
    // the redirect chain happened to land.
    let canonical = extract_canonical_url(&body);

    let resolved = match canonical {
        Some(u) => u,
        None => {
            let still_on_share = final_url == url
                || final_url.starts_with("https://www.facebook.com/share")
                || final_url.starts_with("http://www.facebook.com/share");
            if still_on_share {
                return Ok(ResolvedShare {
                    path: String::new(),
                });
            }
            final_url.clone()
        }
    };

    let stripped = facebook_path_from_url(&resolved).unwrap_or_else(|| {
        resolved
            .trim_start_matches("https://www.facebook.com/")
            .trim_start_matches("http://www.facebook.com/")
            .to_owned()
    });
    tracing::info!(
        path = %path,
        account = %share_account_label(fetcher, account_index),
        status = %status,
        final_url = %final_url,
        resolved = %stripped,
        len = body.len(),
        response_ms,
        read_ms,
        total_ms = started.elapsed().as_millis(),
        "share body resolve done"
    );
    Ok(ResolvedShare { path: stripped })
}

async fn resolve_share_link_head(
    fetcher: &Fetcher,
    path: &str,
    account_index: Option<usize>,
) -> Option<String> {
    let started = Instant::now();
    let url = ensure_absolute(path);
    let mut req = fetcher.client().head(&url);
    for (k, v) in HEADERS {
        req = req.header(*k, *v);
    }
    req = attach_share_identity(fetcher, req, account_index, SHARE_HEAD_USER_AGENT);
    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::debug!(
                path = %path,
                account = %share_account_label(fetcher, account_index),
                error = %e,
                elapsed_ms = started.elapsed().as_millis(),
                "share head resolve failed"
            );
            return None;
        }
    };
    let status = resp.status();
    let final_url = resp.url().to_string();
    if final_url == url {
        tracing::info!(
            path = %path,
            account = %share_account_label(fetcher, account_index),
            status = %status,
            final_url = %final_url,
            resolved = ?Option::<String>::None,
            elapsed_ms = started.elapsed().as_millis(),
            "share head resolve done"
        );
        return None;
    }
    let resolved = facebook_path_from_url(&final_url);
    tracing::info!(
        path = %path,
        account = %share_account_label(fetcher, account_index),
        status = %status,
        final_url = %final_url,
        resolved = ?resolved,
        elapsed_ms = started.elapsed().as_millis(),
        "share head resolve done"
    );
    resolved
}

fn share_account_label(fetcher: &Fetcher, account_index: Option<usize>) -> String {
    let guard = fetcher.cookies.load();
    account_index
        .and_then(|i| guard.label_at(i))
        .unwrap_or("")
        .to_owned()
}

fn attach_share_identity(
    fetcher: &Fetcher,
    req: RequestBuilder,
    account_index: Option<usize>,
    fallback_ua: &'static str,
) -> RequestBuilder {
    let Some(account_index) = account_index else {
        return req.header("user-agent", fallback_ua);
    };
    let guard = fetcher.cookies.load();
    let Some(acc) = guard.account_at(account_index) else {
        return req.header("user-agent", fallback_ua);
    };
    let ua = acc.user_agent.as_deref().unwrap_or(DEFAULT_USER_AGENT);
    req.header("cookie", acc.header_value())
        .header("user-agent", ua)
}

fn facebook_path_from_url(raw: &str) -> Option<String> {
    let parsed = Url::parse(raw).ok()?;
    let host = parsed.host_str()?;
    if !matches!(host, "www.facebook.com" | "facebook.com" | "m.facebook.com") {
        return None;
    }
    let path = parsed.path().trim_start_matches('/');
    if path.is_empty() || path.starts_with("share/") {
        return None;
    }

    let mut out = path.to_owned();
    if let Some(q) = parsed.query().filter(|q| !q.is_empty()) {
        out.push('?');
        out.push_str(q);
    }
    Some(out)
}

/// Pull the post's canonical URL out of an FB share-page HTML body. Tries
/// `<link rel="canonical">` first, falls back to `<meta property="og:url">`.
/// Skips values that point back at /share/ to avoid loops.
fn extract_canonical_url(body: &str) -> Option<String> {
    let doc = Html::parse_document(body);
    if let Some(el) = doc.select(&CANONICAL_LINK_SEL).next() {
        if let Some(href) = el.value().attr("href") {
            if !href.contains("/share/") {
                return Some(href.to_owned());
            }
        }
    }
    for el in doc.select(&OG_URL_SEL) {
        if let Some(content) = el.value().attr("content") {
            if !content.contains("/share/") {
                return Some(content.to_owned());
            }
        }
    }
    None
}

fn extract_account_name(doc: &Html, body: &str) -> Option<String> {
    meta_content(doc, r#"meta[property="og:title"]"#)
        .and_then(|s| normalize_account_name(&s))
        .or_else(|| page_title(doc).and_then(|s| normalize_account_name(&s)))
        .or_else(|| current_user_name_from_json_blocks(doc))
        .or_else(|| current_user_name_from_body(body))
}

fn page_title(doc: &Html) -> Option<String> {
    doc.select(&TITLE_SEL)
        .next()
        .map(|el| el.text().collect::<String>())
}

fn normalize_account_name(raw: &str) -> Option<String> {
    let decoded = html_escape::decode_html_entities(raw).to_string();
    let collapsed = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s = collapsed.trim();
    for suffix in [" | Facebook", " - Facebook"] {
        if let Some(stripped) = s.strip_suffix(suffix) {
            s = stripped.trim();
        }
    }
    if s.is_empty() {
        return None;
    }
    let lower = s.to_ascii_lowercase();
    let bad = [
        "facebook",
        "log in",
        "login",
        "sign up",
        "checkpoint",
        "unsupported browser",
        "privacy",
        "error",
        "not found",
    ];
    if bad.iter().any(|needle| lower.contains(needle)) {
        return None;
    }
    Some(s.to_owned())
}

fn cookie_probe_blocked_reason(final_url: &str, body: &str, doc: &Html) -> Option<&'static str> {
    let url = final_url.to_ascii_lowercase();
    if url.contains("/login") {
        return Some("login redirect");
    }
    if url.contains("/checkpoint") {
        return Some("checkpoint redirect");
    }
    if url.contains("/recover") {
        return Some("account recovery redirect");
    }
    if body.contains("login_data") || body.contains("useCometLogInFormQuery") {
        return Some("login wall");
    }
    if let Some(title) = page_title(doc) {
        let lower = title.to_ascii_lowercase();
        if lower.contains("log in") || lower.contains("checkpoint") {
            return Some("login title");
        }
    }
    None
}

fn current_user_name_from_json_blocks(doc: &Html) -> Option<String> {
    for block in get_json_blocks(doc, false) {
        let mut objects = Vec::new();
        jq::enumerate(&block, &mut objects);
        for obj in objects {
            let looks_like_current_user =
                obj.get("ACCOUNT_ID").is_some() || obj.get("USER_ID").is_some();
            if !looks_like_current_user {
                continue;
            }
            for key in ["NAME", "SHORT_NAME", "name"] {
                if let Some(name) = obj
                    .get(key)
                    .and_then(|v| v.as_str())
                    .and_then(normalize_account_name)
                {
                    return Some(name);
                }
            }
        }
    }
    None
}

static CURRENT_USER_NAME_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#""NAME"\s*:\s*"((?:\\.|[^"\\])*)""#).unwrap());

fn current_user_name_from_body(body: &str) -> Option<String> {
    let mut offset = 0;
    while let Some(pos) = body[offset..].find("CurrentUserInitialData") {
        let start = offset + pos;
        if let Some(name) = name_in_body_window(body, start) {
            return Some(name);
        }
        offset = start + "CurrentUserInitialData".len();
    }
    None
}

fn name_in_body_window(body: &str, start: usize) -> Option<String> {
    let mut end = (start + 6000).min(body.len());
    while end > start && !body.is_char_boundary(end) {
        end -= 1;
    }
    let window = &body[start..end];
    let raw = CURRENT_USER_NAME_RE.captures(window)?.get(1)?.as_str();
    let decoded = serde_json::from_str::<String>(&format!("\"{raw}\"")).ok()?;
    normalize_account_name(&decoded)
}

fn meta_content(doc: &Html, selector: &str) -> Option<String> {
    let sel = Selector::parse(selector).ok()?;
    doc.select(&sel)
        .next()
        .and_then(|el| el.value().attr("content"))
        .map(str::to_owned)
}

static LOGIN_HREF_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"/login\b").unwrap());
static LOGIN_META_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)URL\s*=\s*/login[/?]").unwrap());
static CANONICAL_LINK_SEL: Lazy<Selector> =
    Lazy::new(|| Selector::parse(r#"link[rel="canonical"]"#).unwrap());
static REFRESH_META_SEL: Lazy<Selector> =
    Lazy::new(|| Selector::parse(r#"meta[http-equiv="refresh"]"#).unwrap());
static JSON_SCRIPT_SEL: Lazy<Selector> = Lazy::new(|| {
    Selector::parse(r#"script[type="application/json"][data-content-len][data-sjs]"#).unwrap()
});
static OG_URL_SEL: Lazy<Selector> =
    Lazy::new(|| Selector::parse(r#"meta[property="og:url"]"#).unwrap());
static TITLE_SEL: Lazy<Selector> = Lazy::new(|| Selector::parse("title").unwrap());

/// Classify a Facebook response that indicates the request was blocked rather
/// than served. Rate limits are transient; checkpoint/recovery redirects need a
/// human to re-export cookies. Login walls are detected later by `check_or_raise`.
fn classify_block(
    status: reqwest::StatusCode,
    final_url: &str,
    retry_after: Option<u64>,
) -> Option<FacebedError> {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
    {
        return Some(FacebedError::rate_limited(retry_after));
    }
    let lower = final_url.to_ascii_lowercase();
    if lower.contains("/checkpoint") || lower.contains("/recover") {
        return Some(FacebedError::checkpointed());
    }
    None
}

/// Parse a `Retry-After` header expressed in whole seconds. HTTP-date form is
/// ignored; caller falls back to a default cooldown.
fn retry_after_secs(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
}

pub fn probe_page_type(html: &Html, body: &str) -> PageType {
    if let Some(el) = html.select(&CANONICAL_LINK_SEL).next() {
        if let Some(href) = el.value().attr("href") {
            if LOGIN_HREF_RE.is_match(href) {
                return PageType::LoginWall;
            }
        }
    }
    if let Some(el) = html.select(&REFRESH_META_SEL).next() {
        if let Some(content) = el.value().attr("content") {
            if LOGIN_META_RE.is_match(content) {
                return PageType::LoginWall;
            }
        }
    }

    let has_post_data = body.contains("i18n_reaction_count");
    let has_login_preloader =
        body.contains("login_data") || body.contains("useCometLogInFormQuery");

    if has_post_data {
        PageType::HasData
    } else if has_login_preloader {
        PageType::LoginWall
    } else {
        PageType::Unknown
    }
}

pub fn check_or_raise(page: &FetchedPage, post_path: &str) -> FacebedResult<()> {
    match probe_page_type(page.document(), &page.html) {
        PageType::LoginWall => Err(FacebedError::no_data(format!(
            "Facebook served a login wall for {post_path} - content requires authentication"
        ))),
        _ => Ok(()),
    }
}

pub struct JsonBlockText {
    pub text: String,
}

/// Extract every `<script type=application/json data-content-len=X data-sjs>` JSON blob.
/// Sorted by `data-content-len` desc when `sort=true` (Python behavior).
pub fn get_json_block_texts(html: &Html, sort: bool) -> Vec<JsonBlockText> {
    let mut entries: Vec<(i64, String)> = html
        .select(&JSON_SCRIPT_SEL)
        .filter_map(|el| {
            let len: i64 = el.value().attr("data-content-len")?.parse().ok()?;
            Some((len, el.text().collect::<String>()))
        })
        .collect();

    if sort {
        entries.sort_by(|a, b| b.0.cmp(&a.0));
    }

    entries
        .into_iter()
        .map(|(_, text)| JsonBlockText { text })
        .collect()
}

pub fn get_json_blocks(html: &Html, sort: bool) -> Vec<Value> {
    get_json_block_texts(html, sort)
        .into_iter()
        .filter_map(|block| serde_json::from_str(&block.text).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        cookie_probe_blocked_reason, extract_account_name, facebook_path_from_url,
        head_target_usable, is_group_landing_target, is_post_like_share_target, probe_page_type,
        share_resolution_usable, MediaSizeCache, PageType, ResolvedShare, VIDEO_HEAD_CACHE_MAX,
        VIDEO_HEAD_CACHE_TTL,
    };
    use scraper::Html;
    use std::time::{Duration, Instant};

    #[test]
    fn facebook_path_from_url_keeps_query_for_real_targets() {
        assert_eq!(
            facebook_path_from_url("https://www.facebook.com/watch/?v=123&rdid=x"),
            Some("watch/?v=123&rdid=x".into())
        );
    }

    #[test]
    fn fetch_url_guard_allows_facebook_refuses_other_hosts() {
        use crate::error::FacebedError;

        assert_eq!(
            super::facebook_fetch_url("groups/1/posts/2").unwrap(),
            "https://www.facebook.com/groups/1/posts/2"
        );
        assert!(super::facebook_fetch_url("https://m.facebook.com/x").is_ok());
        assert!(matches!(
            super::facebook_fetch_url("https://example.com/x?type=3"),
            Err(FacebedError::NoData(_))
        ));
    }

    #[test]
    fn classify_block_flags_rate_limit_and_checkpoint() {
        use crate::error::FacebedError;
        use reqwest::StatusCode;

        assert!(matches!(
            super::classify_block(
                StatusCode::TOO_MANY_REQUESTS,
                "https://www.facebook.com/x",
                Some(30)
            ),
            Some(FacebedError::RateLimited {
                retry_after: Some(30)
            })
        ));
        assert!(matches!(
            super::classify_block(
                StatusCode::SERVICE_UNAVAILABLE,
                "https://www.facebook.com/x",
                None
            ),
            Some(FacebedError::RateLimited { retry_after: None })
        ));
        assert!(matches!(
            super::classify_block(
                StatusCode::OK,
                "https://www.facebook.com/checkpoint/?next=y",
                None
            ),
            Some(FacebedError::Checkpointed)
        ));
        assert!(super::classify_block(
            StatusCode::OK,
            "https://www.facebook.com/groups/1/posts/2",
            None
        )
        .is_none());
    }

    #[test]
    fn facebook_path_from_url_skips_share_urls() {
        assert_eq!(
            facebook_path_from_url("https://www.facebook.com/share/p/abc/"),
            None
        );
    }

    #[test]
    fn share_v_head_accepts_group_permalink() {
        assert!(is_post_like_share_target(
            "groups/sportsbook6vn/permalink/1351950440127367/?rdid=x"
        ));
        assert!(!is_group_landing_target(
            "groups/sportsbook6vn/permalink/1351950440127367/?rdid=x"
        ));
    }

    #[test]
    fn group_about_is_landing_target() {
        assert!(is_group_landing_target("groups/sportsbook6vn/about/"));
        assert!(is_group_landing_target("groups/sportsbook6vn/"));
        assert!(!is_group_landing_target(
            "groups/sportsbook6vn/permalink/1351950440127367/"
        ));
    }

    #[test]
    fn share_resolve_rejects_group_landing() {
        assert!(!head_target_usable("groups/sportsbook6vn/about/", false));
        let resolved = ResolvedShare {
            path: "groups/sportsbook6vn/about/".into(),
        };
        assert!(!share_resolution_usable(&resolved));
    }

    #[test]
    fn share_v_head_accepts_reel() {
        assert!(head_target_usable("reel/123", true));
        assert!(head_target_usable("reel/123/?rdid=x&share_url=y", true));
        let resolved = ResolvedShare {
            path: "reel/123".into(),
        };
        assert!(share_resolution_usable(&resolved));
    }

    #[test]
    fn account_name_from_og_title() {
        let doc = Html::parse_document(
            r#"<html><head><meta property="og:title" content="Neyako | Facebook"></head></html>"#,
        );
        assert_eq!(extract_account_name(&doc, "").as_deref(), Some("Neyako"));
    }

    #[test]
    fn account_name_rejects_login_page() {
        let doc = Html::parse_document(
            "<html><head><title>Facebook - log in or sign up</title></head></html>",
        );
        assert_eq!(extract_account_name(&doc, ""), None);
    }

    #[test]
    fn cookie_probe_detects_checkpoint() {
        let doc = Html::parse_document("<html><head><title>Checkpoint</title></head></html>");
        assert_eq!(
            cookie_probe_blocked_reason("https://www.facebook.com/checkpoint/", "", &doc),
            Some("checkpoint redirect")
        );
    }

    #[test]
    fn probe_page_type_uses_raw_post_data_fast_path() {
        let body = r#"<script type="application/json">{"i18n_reaction_count":"1K"}</script>"#;
        let doc = Html::parse_document(body);
        assert_eq!(probe_page_type(&doc, body), PageType::HasData);
    }

    #[test]
    fn probe_page_type_detects_raw_login_preloader() {
        let body = r#"<script>{"queryName":"useCometLogInFormQuery","login_data":{}}</script>"#;
        let doc = Html::parse_document(body);
        assert_eq!(probe_page_type(&doc, body), PageType::LoginWall);
    }

    #[test]
    fn media_size_cache_expires_and_bounds_entries() {
        let mut cache = MediaSizeCache::default();
        let now = Instant::now();

        cache.insert("https://video.test/1", Some(123), now);
        assert_eq!(
            cache.get("https://video.test/1", now + Duration::from_secs(1)),
            Some(Some(123))
        );
        assert_eq!(
            cache.get(
                "https://video.test/1",
                now + VIDEO_HEAD_CACHE_TTL + Duration::from_secs(1)
            ),
            None
        );

        for i in 0..=VIDEO_HEAD_CACHE_MAX {
            cache.insert(&format!("https://video.test/{i}"), None, now);
        }
        assert!(cache.entries.len() <= VIDEO_HEAD_CACHE_MAX);
    }

    #[test]
    fn account_name_from_current_user_initial_data() {
        let body = r#"
            <script>
            requireLazy(["CurrentUserInitialData"], function() {});
            {"ACCOUNT_ID":"1","USER_ID":"1","NAME":"Neyako Tran","SHORT_NAME":"Neyako"}
            </script>
        "#;
        let doc = Html::parse_document(body);
        assert_eq!(
            extract_account_name(&doc, body).as_deref(),
            Some("Neyako Tran")
        );
    }
}
