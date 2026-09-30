use crate::ttl_map::TtlMap;
use std::time::{Duration, Instant};

/// How long a rendered embed body is reused before re-scraping. Short on
/// purpose: reaction counts / edits within this window are not reflected.
pub const EMBED_CACHE_TTL: Duration = Duration::from_secs(90);
/// Max distinct cached paths. Past this, the oldest entry is evicted on insert.
pub const EMBED_CACHE_MAX: usize = 512;

/// Rendered embeds, Activity posts and resolved share links, each kept for
/// [`EMBED_CACHE_TTL`].
pub struct EmbedCache {
    embeds: TtlMap<String>,
    activity: TtlMap<crate::parsers::ParsedPost>,
    shares: TtlMap<String>,
}

impl Default for EmbedCache {
    fn default() -> Self {
        Self {
            embeds: TtlMap::new(EMBED_CACHE_TTL, EMBED_CACHE_MAX),
            activity: TtlMap::new(EMBED_CACHE_TTL, EMBED_CACHE_MAX),
            shares: TtlMap::new(EMBED_CACHE_TTL, EMBED_CACHE_MAX),
        }
    }
}

impl EmbedCache {
    pub fn get_share(&mut self, path: &str, now: Instant) -> Option<String> {
        self.shares.get(path, now)
    }

    pub fn insert_share(&mut self, path: String, resolved: String, now: Instant) {
        self.shares.insert(path, resolved, now);
    }

    pub fn get(&mut self, key: &str, now: Instant) -> Option<String> {
        self.embeds.get(key, now)
    }

    pub fn insert(&mut self, key: &str, body: String, now: Instant) {
        self.embeds.insert(key, body, now);
    }

    pub fn get_activity(&mut self, id: &str, now: Instant) -> Option<crate::parsers::ParsedPost> {
        self.activity.get(id, now)
    }

    pub fn insert_activity(&mut self, id: &str, post: crate::parsers::ParsedPost, now: Instant) {
        self.activity.insert(id, post, now);
    }
}
