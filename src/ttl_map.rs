use std::collections::HashMap;
use std::time::{Duration, Instant};

/// String-keyed map whose entries expire, capped at `max` entries. When full,
/// inserting a new key evicts the entry closest to expiry. The eviction scan
/// is O(n), fine for the few hundred entries these caches hold.
pub struct TtlMap<V> {
    entries: HashMap<String, (V, Instant)>,
    ttl: Duration,
    max: usize,
}

impl<V: Clone> TtlMap<V> {
    pub fn new(ttl: Duration, max: usize) -> Self {
        Self {
            entries: HashMap::new(),
            ttl,
            max,
        }
    }

    /// Value for `key` unless expired at `now`; expired entries are dropped.
    pub fn get(&mut self, key: &str, now: Instant) -> Option<V> {
        let (value, expires_at) = self.entries.get(key)?;
        if now <= *expires_at {
            return Some(value.clone());
        }
        self.entries.remove(key);
        None
    }

    pub fn insert(&mut self, key: impl Into<String>, value: V, now: Instant) {
        self.insert_for(key, value, now, self.ttl);
    }

    /// Insert with a TTL other than the map default.
    pub fn insert_for(&mut self, key: impl Into<String>, value: V, now: Instant, ttl: Duration) {
        let key = key.into();
        if self.entries.len() >= self.max && !self.entries.contains_key(&key) {
            if let Some(soonest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, expires_at))| *expires_at)
                .map(|(key, _)| key.clone())
            {
                self.entries.remove(&soonest);
            }
        }
        self.entries.insert(key, (value, now + ttl));
    }
}

#[cfg(test)]
mod tests {
    use super::TtlMap;
    use std::time::{Duration, Instant};

    #[test]
    fn expires_and_evicts_soonest_when_full() {
        let now = Instant::now();
        let ttl = Duration::from_secs(10);
        let mut map = TtlMap::new(ttl, 2);

        map.insert("a", 1, now);
        assert_eq!(map.get("a", now + ttl), Some(1));
        assert_eq!(map.get("a", now + ttl + Duration::from_secs(1)), None);

        map.insert("old", 1, now);
        map.insert("new", 2, now + Duration::from_secs(1));
        map.insert("newest", 3, now + Duration::from_secs(2));
        assert_eq!(map.get("old", now), None);
        assert_eq!(map.get("new", now), Some(2));
        assert_eq!(map.get("newest", now), Some(3));
    }
}
