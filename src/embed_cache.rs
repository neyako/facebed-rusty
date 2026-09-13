use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long a rendered embed body is reused before re-scraping. Short on
/// purpose: reaction counts / edits within this window are not reflected.
pub const EMBED_CACHE_TTL: Duration = Duration::from_secs(90);
/// Max distinct cached paths. Past this, the oldest entry is evicted on insert.
pub const EMBED_CACHE_MAX: usize = 512;

#[derive(Default)]
pub struct EmbedCache {
    entries: HashMap<String, CachedEmbed>,
    activity_entries: HashMap<String, CachedActivity>,
    share_paths: HashMap<String, (String, Instant)>,
}

struct CachedEmbed {
    body: String,
    stored_at: Instant,
}

struct CachedActivity {
    post: crate::parsers::ParsedPost,
    stored_at: Instant,
}

impl EmbedCache {
    pub fn get_share(&mut self, path: &str, now: Instant) -> Option<String> {
        let (resolved, at) = self.share_paths.get(path)?;
        if now.duration_since(*at) <= EMBED_CACHE_TTL {
            return Some(resolved.clone());
        }
        self.share_paths.remove(path);
        None
    }

    pub fn insert_share(&mut self, path: String, resolved: String, now: Instant) {
        if self.share_paths.len() >= EMBED_CACHE_MAX && !self.share_paths.contains_key(&path) {
            if let Some(oldest) = self
                .share_paths
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(key, _)| key.clone())
            {
                self.share_paths.remove(&oldest);
            }
        }
        self.share_paths.insert(path, (resolved, now));
    }

    /// Return a cached body for `key` if present and not expired. Expired
    /// entries are removed on access.
    pub fn get(&mut self, key: &str, now: Instant) -> Option<String> {
        let entry = self.entries.get(key)?;
        if now.duration_since(entry.stored_at) <= EMBED_CACHE_TTL {
            return Some(entry.body.clone());
        }
        self.entries.remove(key);
        None
    }

    pub fn insert(&mut self, key: &str, body: String, now: Instant) {
        if self.entries.len() >= EMBED_CACHE_MAX && !self.entries.contains_key(key) {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.stored_at)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            key.to_owned(),
            CachedEmbed {
                body,
                stored_at: now,
            },
        );
    }

    pub fn get_activity(&mut self, id: &str, now: Instant) -> Option<crate::parsers::ParsedPost> {
        let entry = self.activity_entries.get(id)?;
        if now.duration_since(entry.stored_at) <= EMBED_CACHE_TTL {
            return Some(entry.post.clone());
        }
        self.activity_entries.remove(id);
        None
    }

    pub fn insert_activity(&mut self, id: &str, post: crate::parsers::ParsedPost, now: Instant) {
        if self.activity_entries.len() >= EMBED_CACHE_MAX && !self.activity_entries.contains_key(id)
        {
            if let Some(oldest) = self
                .activity_entries
                .iter()
                .min_by_key(|(_, entry)| entry.stored_at)
                .map(|(key, _)| key.clone())
            {
                self.activity_entries.remove(&oldest);
            }
        }
        self.activity_entries.insert(
            id.to_owned(),
            CachedActivity {
                post,
                stored_at: now,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::ParsedPost;

    fn activity_post() -> ParsedPost {
        ParsedPost {
            author_name: "Author".into(),
            author_id: None,
            author_handle: None,
            author_avatar_url: None,
            context: None,
            text: "Post body".into(),
            allow_discord_markdown: false,
            image_links: Vec::new(),
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
    fn embed_cache_expires_and_bounds_entries() {
        let mut cache = EmbedCache::default();
        let now = Instant::now();

        cache.insert("groups/1/posts/2", "<html>A</html>".into(), now);
        assert_eq!(
            cache.get("groups/1/posts/2", now + Duration::from_secs(1)),
            Some("<html>A</html>".into())
        );
        assert_eq!(
            cache.get(
                "groups/1/posts/2",
                now + EMBED_CACHE_TTL + Duration::from_secs(1)
            ),
            None
        );

        for i in 0..=EMBED_CACHE_MAX {
            cache.insert(&format!("p/{i}"), "x".into(), now);
        }
        assert!(cache.entries.len() <= EMBED_CACHE_MAX);
    }

    #[test]
    fn share_cache_expires_and_bounds_entries() {
        let mut cache = EmbedCache::default();
        let now = Instant::now();
        cache.insert_share("share/r/x".into(), "groups/g/posts/1".into(), now);
        assert_eq!(
            cache.get_share("share/r/x", now),
            Some("groups/g/posts/1".into())
        );
        assert!(cache
            .get_share("share/r/x", now + EMBED_CACHE_TTL + Duration::from_secs(1))
            .is_none());
    }

    #[test]
    fn activity_cache_expires_and_bounds_entries() {
        let mut cache = EmbedCache::default();
        let now = Instant::now();
        let post = activity_post();

        cache.insert_activity("123", post.clone(), now);
        assert_eq!(
            cache.get_activity("123", now).map(|cached| cached.text),
            Some(post.text)
        );
        assert!(cache
            .get_activity("123", now + EMBED_CACHE_TTL + Duration::from_secs(1))
            .is_none());

        for i in 0..=EMBED_CACHE_MAX {
            cache.insert_activity(
                &format!("activity/{i}"),
                activity_post(),
                now + Duration::from_millis(i as u64),
            );
        }
        assert!(cache.activity_entries.len() <= EMBED_CACHE_MAX);
        assert!(cache
            .get_activity(
                "activity/0",
                now + Duration::from_millis((EMBED_CACHE_MAX + 1) as u64)
            )
            .is_none());
        assert_eq!(
            cache
                .get_activity(
                    &format!("activity/{}", EMBED_CACHE_MAX),
                    now + Duration::from_millis((EMBED_CACHE_MAX + 1) as u64)
                )
                .map(|cached| cached.text),
            Some("Post body".into())
        );
    }
}
