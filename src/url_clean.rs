use once_cell::sync::Lazy;
use std::collections::HashSet;
use url::Url;

const FB_BASE: &str = "https://www.facebook.com";

/// Query keys we strip from any URL before fetching. Mobile/sharing junk.
static DROP_KEYS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "fs",
        "mibextid",
        "rdid",
        "share_url",
        "paipv",
        "_rdr",
        "eav",
        "refsrc",
        "_ft_",
        "__tn__",
        "__cft__",
        "__cft__[0]",
        "__cft__%5B0%5D",
        "__xts__",
        "__xts__[0]",
        "fref",
        "hc_ref",
        "hc_location",
        "notif_id",
        "notif_t",
        "ref",
        "sfnsn",
        "wtsid",
    ]
    .iter()
    .copied()
    .collect()
});

/// Returns the path-and-query portion of the cleaned URL, with `https://www.facebook.com/` stripped.
/// Accepts an absolute facebook URL, a path with query, or a bare path.
pub fn clean_path(input: &str) -> String {
    let absolute = ensure_absolute(input);
    let parsed = match Url::parse(&absolute) {
        Ok(u) => u,
        Err(_) => return strip_prefix(input).to_owned(),
    };

    let kept: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| !DROP_KEYS.contains(k.as_ref()) && !k.starts_with("__cft__"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    let mut path = parsed.path().trim_start_matches('/').to_owned();
    if !kept.is_empty() {
        let mut tmp = Url::parse(FB_BASE).unwrap();
        {
            let mut qp = tmp.query_pairs_mut();
            for (k, v) in &kept {
                qp.append_pair(k, v);
            }
        }
        if let Some(q) = tmp.query() {
            path.push('?');
            path.push_str(q);
        }
    }
    path
}

pub fn ensure_absolute(input: &str) -> String {
    if input.starts_with("http://") || input.starts_with("https://") {
        input.to_owned()
    } else {
        format!("{}/{}", FB_BASE, input.trim_start_matches('/'))
    }
}

fn strip_prefix(s: &str) -> &str {
    s.trim_start_matches("https://www.facebook.com/")
        .trim_start_matches("http://www.facebook.com/")
        .trim_start_matches('/')
}

/// If the input URL has a `share_url=` param, return that URL (after recursive clean).
/// Useful when mobile wraps the real URL in tracking shell.
pub fn extract_share_url(input: &str) -> Option<String> {
    let absolute = ensure_absolute(input);
    let parsed = Url::parse(&absolute).ok()?;
    for (k, v) in parsed.query_pairs() {
        if k == "share_url"
            && (v.starts_with("https://www.facebook.com/")
                || v.starts_with("http://www.facebook.com/"))
        {
            return Some(clean_path(&v));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tracking() {
        let cleaned = clean_path("reel/4021106584686167/?fs=e&mibextid=wwXIfr&rdid=fNtJxhQSQmj6KME6&share_url=https%3A%2F%2Fwww.facebook.com%2Fshare%2Fr%2F18gjrJPZqj%2F");
        assert_eq!(cleaned, "reel/4021106584686167/");
    }

    #[test]
    fn keeps_legitimate_query() {
        let cleaned = clean_path("story.php?story_fbid=123&id=456&fs=e");
        assert!(cleaned.contains("story_fbid=123"));
        assert!(cleaned.contains("id=456"));
        assert!(!cleaned.contains("fs="));
    }

    #[test]
    fn share_url_extraction() {
        let s = extract_share_url(
            "reel/123/?share_url=https%3A%2F%2Fwww.facebook.com%2Fshare%2Fr%2Fabc%2F",
        );
        assert_eq!(s, Some("share/r/abc/".to_string()));
    }
}
