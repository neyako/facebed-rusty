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

impl FetchedPage {
    pub fn parse(&self) -> Html {
        Html::parse_document(&self.html)
    }
}

const HEADERS: &[(&str, &str)] = &[
    ("accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/jxl,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7"),
    ("accept-language", "en-US,en;q=0.9"),
    ("cache-control", "no-cache"),
    ("pragma", "no-cache"),
    ("priority", "u=0, i"),
    ("sec-fetch-mode", "navigate"),
    ("sec-fetch-site", "none"),
    ("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.36"),
];

impl Fetcher {
    pub fn new(cookies: Arc<CookieJar>) -> anyhow::Result<Self> {
        let client = Client::builder()
            .gzip(true)
            .brotli(true)
            .timeout(std::time::Duration::from_secs(20))
            .build()?;
        Ok(Self { client, cookies })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Fetch a Facebook path. Optionally attach cookies. Raises NoData on login walls.
    pub async fn fetch(&self, post_path: &str, use_cookies: bool) -> FacebedResult<FetchedPage> {
        let url = ensure_absolute(post_path);
        let mut req = self.client.get(&url);
        for (k, v) in HEADERS {
            req = req.header(*k, *v);
        }
        if use_cookies {
            if let Some(acc) = self.cookies.next_account() {
                req = req.header("cookie", acc.header_value());
            }
        }
        let resp = req.send().await?;
        let html = resp.text().await?;
        let page = FetchedPage { url, html };
        check_or_raise(&page, post_path)?;
        Ok(page)
    }
}

/// Resolve a `/share/v/...` or `/share/[pr]/...` link.
/// First tries the HTTP redirect chain; falls back to parsing `og:url` from the
/// share page when FB serves it as a 200 (no redirect) — that page still carries
/// the canonical post URL.
/// Returns the resolved path (no host), or empty if neither resolves off /share/.
pub async fn resolve_share_link(fetcher: &Fetcher, path: &str) -> FacebedResult<String> {
    let url = ensure_absolute(path);
    let mut req = fetcher.client().get(&url);
    for (k, v) in HEADERS {
        req = req.header(*k, *v);
    }
    let resp = req.send().await?;
    let final_url = resp.url().to_string();
    let body = resp.text().await?;

    let still_on_share = final_url == url
        || final_url.starts_with("https://www.facebook.com/share")
        || final_url.starts_with("http://www.facebook.com/share");

    let resolved = if !still_on_share {
        final_url
    } else if let Some(u) = extract_canonical_url(&body) {
        u
    } else {
        return Ok(String::new());
    };

    let stripped = resolved
        .trim_start_matches("https://www.facebook.com/")
        .trim_start_matches("http://www.facebook.com/")
        .to_owned();
    Ok(stripped)
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
    let sel = Selector::parse(r#"script[type="application/json"][data-content-len][data-sjs]"#).unwrap();
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
