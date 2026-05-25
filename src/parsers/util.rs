use crate::error::FacebedError;
use crate::jq;
use serde_json::Value;

/// Python `Utils.human_format`. Integer → `1.23K`/`4.5M` style, non-int → unchanged.
pub fn human_format(num: &Value) -> String {
    let n_opt = match num {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    };
    let Some(n) = n_opt else {
        return match num {
            Value::String(s) => s.clone(),
            _ => num.to_string(),
        };
    };

    let mut f = n as f64;
    let mut magnitude = 0;
    while f.abs() >= 1000.0 {
        magnitude += 1;
        f /= 1000.0;
    }
    let suffix = ["", "K", "M", "B", "T"]
        .get(magnitude)
        .copied()
        .unwrap_or("T");
    let mut s = format!("{:.3}", (f * 1000.0).round() / 1000.0);
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    format!("{}{}", s, suffix)
}

pub fn val_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        _ => v.to_string(),
    }
}

pub fn val_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

pub fn val_str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// `Story` from Python — used by JsonParser. Recursive: attached_story is the shared/quoted post.
pub struct Story<'a> {
    pub author_name: String,
    pub text: String,
    pub image_links: Vec<String>,
    pub video_links: Vec<String>,
    pub url: String,
    pub author_id: String,
    pub attached: Option<Box<Story<'a>>>,
    _phantom: std::marker::PhantomData<&'a ()>,
}

impl<'a> Story<'a> {
    pub fn from_json(story_json: &Value) -> Result<Self, FacebedError> {
        let actors = story_json
            .get("actors")
            .and_then(|a| a.as_array())
            .ok_or_else(|| FacebedError::parse("story.actors missing"))?;
        let actor = actors
            .first()
            .ok_or_else(|| FacebedError::parse("story.actors[0] missing"))?;
        let author_name = val_str_at(actor, "name").unwrap_or("").to_owned();
        let author_id = match actor.get("id") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        let mut text = story_json
            .get("message")
            .and_then(|m| m.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_owned();
        if let Some((title, link_url)) = extract_link_card(story_json) {
            if title.is_empty() {
                text.push_str(&format!("\n🔗 {}", link_url));
            } else {
                text.push_str(&format!("\n🔗 {}: {}", title, link_url));
            }
        }
        let url = val_str_at(story_json, "wwwURL").unwrap_or("").to_owned();

        let mut image_links = images_from_post(story_json);
        let mut video_links = videos_from_post(story_json);

        let attached = match story_json.get("attached_story") {
            Some(att) if att.is_object() && att.get("actors").is_some() => {
                let inner = Story::from_json(att)?;
                for u in &inner.image_links {
                    if !image_links.contains(u) {
                        image_links.push(u.clone());
                    }
                }
                for u in &inner.video_links {
                    if !video_links.contains(u) {
                        video_links.push(u.clone());
                    }
                }
                Some(Box::new(inner))
            }
            _ => None,
        };

        Ok(Self {
            author_name,
            text,
            image_links,
            video_links,
            url,
            author_id,
            attached,
            _phantom: std::marker::PhantomData,
        })
    }

    pub fn get_text(&self) -> String {
        match &self.attached {
            Some(a) => format!("{}\n╰┈➤ {}\n{}", self.text, a.author_name, a.text),
            None => self.text.clone(),
        }
    }
}

/// Image URLs from any post-json subtree. Mirrors Story.get_image_links_post_json.
pub fn images_from_post(post_json: &Value) -> Vec<String> {
    let all_attachments = jq::all(post_json, "attachment");

    // multi-image: any attachment with `*subattachments` containing `viewer_image`
    for attachment_set in &all_attachments {
        let sub: Vec<&Value> = match attachment_set {
            Value::Object(map) => map
                .iter()
                .filter(|(k, v)| k.ends_with("subattachments") && v.get("nodes").is_some())
                .map(|(_, v)| v)
                .collect(),
            _ => Vec::new(),
        };
        if sub.is_empty() {
            continue;
        }
        let max_count = sub
            .iter()
            .filter_map(|s| s.get("nodes").and_then(|n| n.as_array()).map(|a| a.len()))
            .max()
            .unwrap_or(0);
        let candidates: Vec<&Value> = sub
            .into_iter()
            .filter(|s| {
                s.get("nodes")
                    .and_then(|n| n.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0)
                    == max_count
                    && !jq::all(s, "viewer_image").is_empty()
            })
            .collect();
        if let Some(first) = candidates.into_iter().next() {
            let imgs: Vec<String> = jq::all(first, "viewer_image")
                .into_iter()
                .filter_map(|v| val_str_at(v, "uri").map(str::to_owned))
                .collect();
            if !imgs.is_empty() {
                return imgs;
            }
        }
    }

    // single-set: attachment with "media" but not a Sticker
    for attachment_set in &all_attachments {
        if attachment_set.get("media").is_some() {
            let dumped = attachment_set.to_string();
            if dumped.contains("'__typename': 'Sticker'") {
                continue;
            }
            let imgs: Vec<String> = jq::all(attachment_set, "photo_image")
                .into_iter()
                .filter_map(|v| val_str_at(v, "uri").map(str::to_owned))
                .collect();
            if !imgs.is_empty() {
                return imgs;
            }
        }
    }

    // fallback: comet_photo_attachment_resolution_renderer.image.uri
    for aa in jq::all(post_json, "comet_photo_attachment_resolution_renderer") {
        if let Some(uri) = aa
            .get("image")
            .and_then(|i| i.get("uri"))
            .and_then(|s| s.as_str())
        {
            return vec![uri.to_owned()];
        }
    }
    Vec::new()
}

/// Video URLs from any post-json subtree.
pub fn videos_from_post(post_json: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for attachment_set in jq::all(post_json, "attachment") {
        if let Some(link) = video_link_in_node(attachment_set) {
            if !out.contains(&link) {
                out.push(link);
            }
        }
    }
    out
}

/// Bug-1 fix lives here: probe the modern progressive_url shape AND the legacy
/// browser_native_*_url shape, since FB may serve either depending on context.
pub fn video_link_in_node(node: &Value) -> Option<String> {
    // Modern: videoDeliveryResponseFragment.videoDeliveryResponseResult.progressive_urls[].progressive_url
    for fragment in jq::all(node, "videoDeliveryResponseFragment") {
        for url_obj in jq::all(fragment, "progressive_url") {
            if let Some(s) = url_obj.as_str() {
                if !s.is_empty() {
                    return Some(s.to_owned());
                }
            }
        }
    }
    // Legacy:
    for legacy in jq::all(node, "videoDeliveryLegacyFields") {
        if legacy.is_null() {
            continue;
        }
        for key in ["browser_native_hd_url", "browser_native_sd_url"] {
            if let Some(v) = jq::first(legacy, key).and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    return Some(v.to_owned());
                }
            }
        }
    }
    // Direct: playable_url (used by Stories and some reels)
    if let Some(v) = jq::first(node, "playable_url_quality_hd").and_then(|v| v.as_str()) {
        if !v.is_empty() {
            return Some(v.to_owned());
        }
    }
    if let Some(v) = jq::first(node, "playable_url").and_then(|v| v.as_str()) {
        if !v.is_empty() {
            return Some(v.to_owned());
        }
    }
    None
}

/// Best-effort thumbnail/preview image URL for a video node. Walks the common
/// shapes FB uses: `preferred_thumbnail.image.uri`, `image.uri`, and
/// `thumbnailImage.uri`. Returns the first non-empty hit.
pub fn thumbnail_in_node(node: &Value) -> Option<String> {
    let candidates = [
        &["preferred_thumbnail", "image", "uri"][..],
        &["thumbnailImage", "uri"][..],
        &["image", "uri"][..],
    ];
    for path in candidates {
        let mut cur = node;
        let mut ok = true;
        for seg in path {
            match cur.get(*seg) {
                Some(v) => cur = v,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            if let Some(s) = cur.as_str() {
                if !s.is_empty() {
                    return Some(s.to_owned());
                }
            }
        }
    }
    // Fallback: scan any nested `preferred_thumbnail` / `thumbnailImage`.
    for key in ["preferred_thumbnail", "thumbnailImage"] {
        if let Some(t) = jq::first(node, key) {
            if let Some(uri) = t
                .get("image")
                .and_then(|i| i.get("uri"))
                .and_then(|s| s.as_str())
            {
                if !uri.is_empty() {
                    return Some(uri.to_owned());
                }
            }
            if let Some(uri) = t.get("uri").and_then(|s| s.as_str()) {
                if !uri.is_empty() {
                    return Some(uri.to_owned());
                }
            }
        }
    }
    None
}

/// Extract the first link-card attachment from a story subtree: `(title, url)`
/// for `attachment.target.external_url`. FB posts that are just a shared link
/// (with optional preview title) have no `message.text`, so without this the
/// embed body comes back empty. `title` may be empty when FB renders only the
/// URL. Scan is recursive — same behavior as the upstream JS reference.
pub fn extract_link_card(story_json: &Value) -> Option<(String, String)> {
    for attachment in jq::all(story_json, "attachment") {
        let Some(target) = attachment.get("target") else {
            continue;
        };
        let Some(url) = target.get("external_url").and_then(|u| u.as_str()) else {
            continue;
        };
        if url.is_empty() {
            continue;
        }
        let title = attachment
            .get("title_with_entities")
            .and_then(|t| t.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_owned();
        return Some((title, url.to_owned()));
    }
    None
}

pub fn interaction_counts(post_json: &Value) -> Result<(String, String, String), FacebedError> {
    let pf = jq::first(post_json, "comet_ufi_summary_and_actions_renderer")
        .ok_or_else(|| FacebedError::parse("missing comet_ufi_summary_and_actions_renderer"))?;
    let fb = pf
        .get("feedback")
        .ok_or_else(|| FacebedError::parse("missing feedback"))?;
    let reactions = fb
        .get("i18n_reaction_count")
        .map(val_str)
        .unwrap_or_else(|| "0".into());
    let shares = fb
        .get("i18n_share_count")
        .map(val_str)
        .unwrap_or_else(|| "0".into());
    let comments = fb
        .get("comment_rendering_instance")
        .and_then(|c| c.get("comments"))
        .and_then(|c| c.get("total_count"))
        .map(val_str)
        .unwrap_or_else(|| "0".into());
    Ok((reactions, comments, shares))
}
