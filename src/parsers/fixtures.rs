//! Parser tests against real Facebook pages, captured and minimized so a
//! schema change shows up here instead of as a broken embed in Discord.
//!
//! Capture a fixture (needs a cookie that can see the post; public Page
//! content only, the repo is public):
//!
//! ```text
//! facebed -c config.yaml --dump '<fb path>' --dump-dir /tmp/dump
//! FIXTURE_HTML=/tmp/dump/page.html FIXTURE_KIND=reel FIXTURE_PATH='<fb path>' \
//! FIXTURE_REDACT='<account c_user id>,<account name>' \
//! FIXTURE_OUT=src/parsers/fixtures/<name>.html \
//!   cargo test --release capture_fixture -- --ignored --nocapture
//! ```
//!
//! The tool keeps only the head tags, JSON blocks and JSON fields whose
//! removal changes the parser's output, redacts `FIXTURE_REDACT`, strips
//! fbcdn query strings, and refuses to write if a session token survives.
//! Still read the file before committing it.

use super::{json_post, photocom, reels, single_photo, video_watch, ParsedPost, ParserCtx};
use crate::error::FacebedResult;
use crate::fetch::FetchedPage;
use serde_json::Value;
use std::sync::Arc;

fn ctx() -> ParserCtx {
    let cookies = Arc::new(arc_swap::ArcSwap::from_pointee(
        crate::cookies::CookieJar::empty(),
    ));
    ParserCtx {
        fetcher: Arc::new(crate::fetch::Fetcher::new(cookies.clone()).unwrap()),
        cookies,
        config: Arc::new(arc_swap::ArcSwap::from_pointee(
            crate::config::Config::default(),
        )),
    }
}

fn parse(kind: &str, path: &str, page: &FetchedPage) -> FacebedResult<ParsedPost> {
    match kind {
        "reel" => reels::parse_reel(&ctx(), path, page),
        "watch" => video_watch::parse_page(path, page),
        "photo" => single_photo::parse_page(path, page),
        "photocom" => photocom::parse_page(path, page),
        "post" => json_post::parse_fetched_post(&ctx(), path, page),
        other => panic!("unknown fixture kind {other}"),
    }
}

fn page(html: String, path: &str) -> FacebedResult<FetchedPage> {
    FetchedPage::from_html(
        crate::url_clean::ensure_absolute(path),
        html,
        path,
        false,
        false,
    )
}

fn fixture(name: &str, kind: &str, path: &str) -> ParsedPost {
    let file = format!(
        "{}/src/parsers/fixtures/{name}.html",
        env!("CARGO_MANIFEST_DIR")
    );
    let html = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{file}: {e}"));
    parse(kind, path, &page(html, path).unwrap()).unwrap()
}

// ---- capture tool ------------------------------------------------------

enum Item {
    Head(String),
    Block { len: String, value: Value },
}

fn build(items: &[Item]) -> String {
    let mut head = String::new();
    let mut body = String::new();
    for item in items {
        match item {
            Item::Head(tag) => head.push_str(tag),
            Item::Block { len, value } => {
                // `<` only occurs inside JSON strings; escaping it keeps a
                // `</script>` in post text from closing the tag.
                let json = value.to_string().replace('<', "\\u003c");
                body.push_str(&format!(
                    "<script type=\"application/json\" data-content-len=\"{len}\" data-sjs>{json}</script>\n"
                ));
            }
        }
    }
    format!("<html><head>{head}</head><body>\n{body}</body></html>\n")
}

fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Depth-first: drop each child of the node at `ptr` if the output survives
/// without it, otherwise keep it and prune inside it.
fn prune(items: &mut [Item], index: usize, ptr: &str, ok: &dyn Fn(&[Item]) -> bool) {
    let Item::Block { value, .. } = &items[index] else {
        return;
    };
    let keys: Vec<String> = match value.pointer(ptr) {
        Some(Value::Object(map)) => map.keys().cloned().collect(),
        Some(Value::Array(list)) => (0..list.len()).rev().map(|i| i.to_string()).collect(),
        _ => return,
    };
    for key in keys {
        let Item::Block { value, .. } = &mut items[index] else {
            return;
        };
        let parent = value.pointer_mut(ptr).unwrap();
        let removed = match parent {
            Value::Object(map) => map.remove(&key).map(|v| (v, None)),
            Value::Array(list) => {
                let i: usize = key.parse().unwrap();
                Some((list.remove(i), Some(i)))
            }
            _ => None,
        };
        let Some((child, position)) = removed else {
            continue;
        };
        if ok(items) {
            continue;
        }
        let Item::Block { value, .. } = &mut items[index] else {
            return;
        };
        match (value.pointer_mut(ptr).unwrap(), position) {
            (Value::Object(map), None) => {
                map.insert(key.clone(), child);
            }
            (Value::Array(list), Some(i)) => list.insert(i, child),
            _ => unreachable!(),
        }
        prune(items, index, &format!("{ptr}/{}", escape_pointer(&key)), ok);
    }
}

fn strip_fbcdn_queries(html: &str) -> String {
    let re = regex::Regex::new(r#"(https?:[^"'\s<>]*fbcdn\.net[^"'\s<>?]*)\?[^"'\s<>]*"#).unwrap();
    re.replace_all(html, "$1").into_owned()
}

#[test]
#[ignore = "capture tool, see module docs"]
fn capture_fixture() {
    let var = |key: &str| std::env::var(key).unwrap_or_else(|_| panic!("set {key}"));
    let (source, kind, path, out) = (
        var("FIXTURE_HTML"),
        var("FIXTURE_KIND"),
        var("FIXTURE_PATH"),
        var("FIXTURE_OUT"),
    );
    let redact: Vec<String> = std::env::var("FIXTURE_REDACT")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();

    let raw = std::fs::read_to_string(&source).unwrap();
    let document = scraper::Html::parse_document(&raw);
    let head_sel = scraper::Selector::parse(
        r#"link[rel="canonical"], meta[property^="og:"], meta[http-equiv="refresh"], title"#,
    )
    .unwrap();
    let block_sel =
        scraper::Selector::parse(r#"script[type="application/json"][data-content-len][data-sjs]"#)
            .unwrap();
    let mut items: Vec<Item> = document
        .select(&head_sel)
        .map(|el| Item::Head(el.html()))
        .collect();
    items.extend(document.select(&block_sel).filter_map(|el| {
        let value = serde_json::from_str(&el.text().collect::<String>()).ok()?;
        Some(Item::Block {
            len: el.value().attr("data-content-len")?.to_owned(),
            value,
        })
    }));

    let output = |items: &[Item]| {
        page(build(items), &path)
            .and_then(|p| parse(&kind, &path, &p))
            .map(|post| format!("{post:?}"))
            .ok()
    };
    let baseline = output(&items).expect("parser must succeed on the raw page");
    let ok = |items: &[Item]| output(items).as_ref() == Some(&baseline);

    for index in (0..items.len()).rev() {
        let item = items.remove(index);
        if !ok(&items) {
            items.insert(index, item);
        }
    }
    for index in 0..items.len() {
        prune(&mut items, index, "", &ok);
    }

    let mut html = strip_fbcdn_queries(&build(&items));
    for secret in &redact {
        html = html.replace(secret.as_str(), "REDACTED");
    }
    for token in [
        "fb_dtsg",
        "DTSGInit",
        "\"LSD\"",
        "async_get_token",
        "access_token",
        "__spin",
        "c_user",
        "\"xs\"",
    ] {
        assert!(!html.contains(token), "{token} survived minimization");
    }
    let post = parse(&kind, &path, &page(html.clone(), &path).unwrap())
        .expect("parser must still succeed after scrubbing");
    std::fs::write(&out, &html).unwrap();
    println!("wrote {out} ({} bytes)\n{post:#?}", html.len());
}

// ---- fixtures ------------------------------------------------------------

#[test]
fn reel_fixture() {
    let post = fixture("reel", "reel", "reel/1052271451146452");
    assert_eq!(post.author_name, "Crunchyroll");
    assert_eq!(post.author_handle.as_deref(), Some("Crunchyroll"));
    assert!(post.text.starts_with("A new season of anime"));
    assert_eq!(
        (
            post.likes.as_str(),
            post.comments.as_str(),
            post.shares.as_str()
        ),
        ("20.096", "263", "1.900")
    );
    assert_eq!(post.video_links.len(), 1);
    assert!(post.video_links[0].ends_with(".mp4"));
    assert!(post.thumbnail.is_some());
}

#[test]
fn page_video_fixture() {
    let path = "tintucvtv24/videos/1510336304185822/";
    let post = fixture("page_video", "watch", path);
    assert_eq!(post.author_name, "VTV24");
    assert_eq!(post.url, crate::url_clean::ensure_absolute(path));
    assert_eq!(
        (post.likes.as_str(), post.comments.as_str()),
        ("101.741", "9.629")
    );
    assert_eq!(post.video_links.len(), 1);
    assert!(post.thumbnail.is_some());
}

#[test]
fn page_post_fixture() {
    let post = fixture(
        "page_post",
        "post",
        "tintucvtv24/posts/pfbid0ugtJjQkwRG3twcscqFMkNieT8KdwBAtnXDX7QRpPCmmyUaaZirgEnVQ34WeFPWDBl",
    );
    assert_eq!(post.author_name, "VTV24");
    assert!(post.text.starts_with("Ban Tổ chức nhận trách nhiệm"));
    assert!(post.url.contains("/tintucvtv24/posts/pfbid"));
    assert_eq!(
        (
            post.likes.as_str(),
            post.comments.as_str(),
            post.shares.as_str()
        ),
        ("119.529", "7.347", "1.737")
    );
    assert!(!post.image_links.is_empty());
    assert!(post.video_links.is_empty());
}

#[test]
fn album_photo_fixture_renders_through_both_photo_parsers() {
    let path = "photo.php?fbid=1745486344243984&set=a.358099056316060&type=3";
    let single = fixture("album_photo", "photo", path);
    assert_eq!(single.author_name, "VTV Toạ độ việc làm");
    assert!(single.text.starts_with("VIỆT NAM CÓ 1.4 TRIỆU THANH NIÊN"));
    assert_eq!(
        (
            single.likes.as_str(),
            single.comments.as_str(),
            single.shares.as_str()
        ),
        ("879", "207", "91")
    );
    assert_eq!(single.image_links.len(), 1);
    // type=3 routes to the comment-image parser; with no attached comment
    // it must fall back to the same single-photo embed.
    let via_photocom = fixture("album_photo", "photocom", path);
    assert_eq!(format!("{via_photocom:?}"), format!("{single:?}"));
}
