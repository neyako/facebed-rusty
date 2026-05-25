use crate::cookies::CookieJar;
use crate::error::{FacebedError, FacebedResult};
use crate::jq;
use crate::url_clean::ensure_absolute;
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Client;
use scraper::{Html, Selector};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

tokio::task_local! {
    /// When set, [`Fetcher::fetch`] uses this account index (modulo account count)
    /// instead of the primary account. Used by the per-request retry loop to
    /// deterministically try fallback cookie accounts.
    pub static ACCOUNT_OVERRIDE: usize;
}

pub struct Fetcher {
    client: Client,
    cookies: Arc<CookieJar>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageType {
    HasData,
    LoginWall,
    Unknown,
}

#[derive(Debug)]
pub struct FetchedPage {
    pub url: String,
    pub html: String,
}

#[derive(Debug, Clone)]
pub struct OgPreview {
    pub title: String,
    pub description: String,
    pub image: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedShare {
    pub path: String,
    pub preview: Option<OgPreview>,
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
    pub fn parse(&self) -> Html {
        Html::parse_document(&self.html)
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

impl Fetcher {
    pub fn new(cookies: Arc<CookieJar>) -> anyhow::Result<Self> {
        let client = Client::builder()
            .gzip(true)
            .brotli(true)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(8))
            .build()?;
        Ok(Self { client, cookies })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub async fn check_cookie_accounts(&self) -> Vec<CookieAccountCheck> {
        let mut checks = Vec::with_capacity(self.cookies.len());
        for i in 0..self.cookies.len() {
            checks.push(self.check_cookie_account(i).await);
        }
        checks
    }

    pub async fn check_cookie_account(&self, account_index: usize) -> CookieAccountCheck {
        let Some(acc) = self.cookies.account_at(account_index) else {
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
        let resp = self.client.head(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.headers()
            .get(reqwest::header::CONTENT_LENGTH)?
            .to_str()
            .ok()?
            .parse()
            .ok()
    }

    /// Fetch a Facebook path. Optionally attach cookies. Raises NoData on login walls.
    pub async fn fetch(&self, post_path: &str, use_cookies: bool) -> FacebedResult<FetchedPage> {
        let url = ensure_absolute(post_path);
        let mut req = self.client.get(&url);
        for (k, v) in HEADERS {
            req = req.header(*k, *v);
        }
        let mut account_label = String::new();
        let mut user_agent: &str = DEFAULT_USER_AGENT;
        if use_cookies {
            let acc = ACCOUNT_OVERRIDE
                .try_with(|i| self.cookies.account_at(*i))
                .ok()
                .flatten()
                .or_else(|| self.cookies.account_at(0));
            if let Some(acc) = acc {
                account_label = acc.label.clone();
                req = req.header("cookie", acc.header_value());
                if let Some(ua) = acc.user_agent.as_deref() {
                    user_agent = ua;
                }
            }
        }
        req = req.header("user-agent", user_agent);
        let resp = req.send().await?;
        let status = resp.status();
        let final_url = resp.url().to_string();
        let html = resp.text().await?;
        tracing::info!(path = %post_path, account = %account_label, status = %status, final_url = %final_url, len = html.len(), "fetch done");
        let page = FetchedPage { url, html };
        check_or_raise(&page, post_path)?;
        Ok(page)
    }
}

/// Resolve a `/share/v/...` or `/share/[pr]/...` link to its canonical
/// content path.
///
/// Fast path mirrors upstream Python for share/p and share/r: HEAD the
/// share URL with a requests-like UA and use the redirect target.
/// Browser/Discord UAs are slower or do not redirect reliably here.
///
/// If HEAD cannot produce an off-/share/ target, fall back to a Discordbot
/// GET and prefer `og:url` / canonical from the response body. share/v uses
/// that fallback directly because FB can redirect Page videos to lossy
/// reel-shaped URLs.
pub async fn resolve_share_link(fetcher: &Fetcher, path: &str) -> FacebedResult<ResolvedShare> {
    let is_share_v = is_share_v_path(path);
    let head_path = resolve_share_link_head(fetcher, path).await;
    if let Some(path) = head_path.as_deref() {
        if !is_share_v || is_post_like_share_target(path) {
            return Ok(ResolvedShare {
                path: path.to_owned(),
                preview: None,
            });
        }
    }

    let resolved = resolve_share_link_body(fetcher, path).await?;
    if (resolved.path.is_empty() || is_group_landing_target(&resolved.path)) && head_path.is_some()
    {
        return Ok(ResolvedShare {
            path: head_path.unwrap(),
            preview: None,
        });
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
        || path.contains("/videos/")
        || (path.starts_with("groups/")
            && (path.contains("/permalink/") || path.contains("/posts/")))
}

fn is_group_landing_target(path: &str) -> bool {
    let parsed = Url::parse(&ensure_absolute(path)).ok();
    let path = parsed
        .as_ref()
        .map(|u| u.path().trim_matches('/'))
        .unwrap_or_else(|| path.trim_matches('/'));
    path.starts_with("groups/") && (path.ends_with("/about") || path.split('/').count() <= 2)
}

async fn resolve_share_link_body(fetcher: &Fetcher, path: &str) -> FacebedResult<ResolvedShare> {
    let url = ensure_absolute(path);
    let mut req = fetcher.client().get(&url);
    for (k, v) in HEADERS {
        req = req.header(*k, *v);
    }
    req = req.header(
        "user-agent",
        "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)",
    );
    let resp = req.send().await?;
    let final_url = resp.url().to_string();
    let body = resp.text().await?;

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
                    preview: extract_og_preview(&body),
                });
            }
            final_url
        }
    };

    let stripped = facebook_path_from_url(&resolved).unwrap_or_else(|| {
        resolved
            .trim_start_matches("https://www.facebook.com/")
            .trim_start_matches("http://www.facebook.com/")
            .to_owned()
    });
    Ok(ResolvedShare {
        path: stripped,
        preview: extract_og_preview(&body),
    })
}

async fn resolve_share_link_head(fetcher: &Fetcher, path: &str) -> Option<String> {
    let url = ensure_absolute(path);
    let mut req = fetcher.client().head(&url);
    for (k, v) in HEADERS {
        req = req.header(*k, *v);
    }
    req = req.header("user-agent", SHARE_HEAD_USER_AGENT);
    let resp = req.send().await.ok()?;
    let final_url = resp.url().to_string();
    if final_url == url {
        return None;
    }
    facebook_path_from_url(&final_url)
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

/// Fetch Facebook's public crawler OG tags without cookies. This is much
/// cheaper than authenticated Comet JSON scraping and is used as a deadline
/// fallback so Discord gets *some* valid embed before it gives up.
pub async fn fetch_public_preview(fetcher: &Fetcher, path: &str) -> Option<OgPreview> {
    let url = ensure_absolute(path);
    let mut req = fetcher.client().get(&url);
    for (k, v) in HEADERS {
        req = req.header(*k, *v);
    }
    req = req.header(
        "user-agent",
        "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)",
    );
    let body = req.send().await.ok()?.text().await.ok()?;
    extract_og_preview(&body)
}

/// Pull the post's canonical URL out of an FB share-page HTML body. Tries
/// `<link rel="canonical">` first, falls back to `<meta property="og:url">`.
/// Skips values that point back at /share/ to avoid loops.
fn extract_canonical_url(body: &str) -> Option<String> {
    let doc = Html::parse_document(body);
    let canon_sel = Selector::parse(r#"link[rel="canonical"]"#).unwrap();
    if let Some(el) = doc.select(&canon_sel).next() {
        if let Some(href) = el.value().attr("href") {
            if !href.contains("/share/") {
                return Some(href.to_owned());
            }
        }
    }
    let og_sel = Selector::parse(r#"meta[property="og:url"]"#).unwrap();
    for el in doc.select(&og_sel) {
        if let Some(content) = el.value().attr("content") {
            if !content.contains("/share/") {
                return Some(content.to_owned());
            }
        }
    }
    None
}

fn extract_og_preview(body: &str) -> Option<OgPreview> {
    let doc = Html::parse_document(body);
    let title_sel = Selector::parse("title").unwrap();
    let page_title = doc
        .select(&title_sel)
        .next()
        .map(|el| el.text().collect::<String>())
        .unwrap_or_default();
    let og_title = meta_content(&doc, r#"meta[property="og:title"]"#).unwrap_or_default();
    let og_description =
        meta_content(&doc, r#"meta[property="og:description"]"#).unwrap_or_default();
    let image = meta_content(&doc, r#"meta[property="og:image"]"#)?;
    let url =
        meta_content(&doc, r#"meta[property="og:url"]"#).or_else(|| extract_canonical_url(body))?;

    let parts = page_title
        .split(" | ")
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "Facebook")
        .collect::<Vec<_>>();
    let title = parts
        .first()
        .copied()
        .unwrap_or(og_title.as_str())
        .trim()
        .to_owned();
    let description = if !og_description.trim().is_empty() {
        og_description.trim().to_owned()
    } else {
        parts
            .get(1)
            .copied()
            .unwrap_or(og_title.as_str())
            .trim()
            .to_owned()
    };

    if title.is_empty() || image.is_empty() || url.is_empty() {
        return None;
    }

    Some(OgPreview {
        title,
        description,
        image,
        url,
    })
}

fn extract_account_name(doc: &Html, body: &str) -> Option<String> {
    meta_content(doc, r#"meta[property="og:title"]"#)
        .and_then(|s| normalize_account_name(&s))
        .or_else(|| page_title(doc).and_then(|s| normalize_account_name(&s)))
        .or_else(|| current_user_name_from_json_blocks(doc))
        .or_else(|| current_user_name_from_body(body))
}

fn page_title(doc: &Html) -> Option<String> {
    let title_sel = Selector::parse("title").ok()?;
    doc.select(&title_sel)
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

pub fn probe_page_type(html: &Html) -> PageType {
    let canonical_sel = Selector::parse("link[rel=canonical]").unwrap();
    if let Some(el) = html.select(&canonical_sel).next() {
        if let Some(href) = el.value().attr("href") {
            if LOGIN_HREF_RE.is_match(href) {
                return PageType::LoginWall;
            }
        }
    }
    let meta_sel = Selector::parse(r#"meta[http-equiv="refresh"]"#).unwrap();
    if let Some(el) = html.select(&meta_sel).next() {
        if let Some(content) = el.value().attr("content") {
            if LOGIN_META_RE.is_match(content) {
                return PageType::LoginWall;
            }
        }
    }

    let mut has_login_preloader = false;
    let mut has_post_data = false;

    for bloc in get_json_blocks(html, false) {
        if !has_login_preloader {
            if let Some(v) = jq::first(&bloc, "login_data") {
                if v.is_object() {
                    has_login_preloader = true;
                }
            }
            if !has_login_preloader {
                let mut tmp = Vec::new();
                jq::enumerate(&bloc, &mut tmp);
                for obj in tmp {
                    if let Some(qn) = obj.get("queryName") {
                        if qn.as_str() == Some("useCometLogInFormQuery") {
                            has_login_preloader = true;
                            break;
                        }
                    }
                }
            }
        }
        if jq::has(&bloc, &["i18n_reaction_count"]) {
            has_post_data = true;
            break;
        }
    }

    if has_post_data {
        PageType::HasData
    } else if has_login_preloader {
        PageType::LoginWall
    } else {
        PageType::Unknown
    }
}

pub fn check_or_raise(page: &FetchedPage, post_path: &str) -> FacebedResult<()> {
    let html = page.parse();
    match probe_page_type(&html) {
        PageType::LoginWall => Err(FacebedError::no_data(format!(
            "Facebook served a login wall for {post_path} - content requires authentication"
        ))),
        _ => Ok(()),
    }
}

/// Extract every `<script type=application/json data-content-len=X data-sjs>` JSON blob.
/// Sorted by `data-content-len` desc when `sort=true` (Python behavior).
pub fn get_json_blocks(html: &Html, sort: bool) -> Vec<Value> {
    let sel =
        Selector::parse(r#"script[type="application/json"][data-content-len][data-sjs]"#).unwrap();
    let mut entries: Vec<(i64, String)> = html
        .select(&sel)
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
        .filter_map(|(_, txt)| serde_json::from_str(&txt).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        cookie_probe_blocked_reason, extract_account_name, facebook_path_from_url,
        is_group_landing_target, is_post_like_share_target,
    };
    use scraper::Html;

    #[test]
    fn facebook_path_from_url_keeps_query_for_real_targets() {
        assert_eq!(
            facebook_path_from_url("https://www.facebook.com/watch/?v=123&rdid=x"),
            Some("watch/?v=123&rdid=x".into())
        );
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
