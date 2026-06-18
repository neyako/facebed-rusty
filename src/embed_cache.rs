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
}

struct CachedEmbed {
    body: String,
    stored_at: Instant,
}

impl EmbedCache {
    /// Return a cached body for `key` if present and not expired. Expired
    /// entries are removed on access.
    pub fn get(&mut self, key: &str, now: Instant) -> Option<String> {
        let Some(entry) = self.entries.get(key) else {
            return None;
        };
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
