use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

/// One cookie entry as exported by Cookie-Editor.
#[derive(Debug, Clone, Deserialize)]
pub struct CookieEntry {
    pub name: String,
    pub value: String,
    #[serde(default, rename = "expirationDate")]
    pub expiration_date: Option<f64>,
}

/// A named set of cookies = one Facebook account.
#[derive(Debug, Clone)]
pub struct CookieAccount {
    pub label: String,
    pub entries: Vec<CookieEntry>,
    cookie_header: String,
    /// Optional UA override for this account. Lets each account look like a
    /// different browser/device to FB, which makes a multi-account setup
    /// look less like a single scraper hammering with rotated cookies.
    pub user_agent: Option<String>,
}

impl CookieAccount {
    fn new(label: String, entries: Vec<CookieEntry>, user_agent: Option<String>) -> Self {
        let cookie_header = entries
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ");
        Self {
            label,
            entries,
            cookie_header,
            user_agent,
        }
    }

    pub fn header_value(&self) -> &str {
        &self.cookie_header
    }

    pub fn any_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as f64)
            .unwrap_or(0.0);
        self.entries
            .iter()
            .any(|c| c.expiration_date.map(|e| e <= now).unwrap_or(false))
    }
}

/// Seconds an account stays in cooldown after a failure. While in cooldown
/// the retry loop skips it on the *first* attempt so we don't waste a slow
/// FB round-trip on an account that's checkpointed / has expired cookies.
/// The account is still tried as a last resort if every other account is
/// also in cooldown, so a transient blip doesn't lock everyone out.
pub const ACCOUNT_COOLDOWN_SECS: u64 = 300;

/// Max distinct scope keys retained in the affinity map. Past this, we drop
/// an arbitrary entry on insert to bound memory. Affinity is a hint, not a
/// correctness invariant, so eviction is cheap.
pub const AFFINITY_CAP: usize = 1024;

/// Number of consecutive failures on the same account that triggers an
/// admin notification. One or two failures can be transient FB blips; three
/// in a row almost always means the cookie is expired/checkpointed and
/// needs human attention.
pub const NOTIFY_FAILURE_THRESHOLD: u64 = 3;

/// Pool of accounts. Empty pool = anonymous fetches.
#[derive(Debug)]
pub struct CookieJar {
    accounts: Vec<CookieAccount>,
    /// Per-account "last failure" unix seconds. Parallel to `accounts`.
    /// Zero means "never failed".
    last_failures: Vec<AtomicU64>,
    /// Per-account count of consecutive failures since last success. Used
    /// to fire an admin notification when an account looks persistently
    /// broken (vs. a one-off transient blip).
    consecutive_failures: Vec<AtomicU64>,
    /// Scope key (e.g. `groups/123`, `user/alice`) -> last-successful account
    /// index. Lets repeat requests for the same group/profile try the known
    /// working account first, while cold requests keep the configured account
    /// priority order.
    affinity: Mutex<HashMap<String, usize>>,
}

impl CookieJar {
    #[cfg(test)]
    pub fn empty() -> Self {
        Self {
            accounts: Vec::new(),
            last_failures: Vec::new(),
            consecutive_failures: Vec::new(),
            affinity: Mutex::new(HashMap::new()),
        }
    }

    /// Load cookies. The given `path` (default `./cookies.json`) is loaded if it exists,
    /// AND the parent directory is scanned for any sibling `cookies*.json` files which are
    /// each loaded as additional accounts. Drop `cookies-alice.json`, `cookies2.json`,
    /// `cookies-neyako.json`, etc. next to `cookies.json` to add accounts without editing
    /// any config — the label is derived from the filename.
    ///
    /// Each file may be either:
    ///   - Cookie-Editor flat array `[{name, value, ...}, ...]` → 1 account, label from filename
    ///   - Multi-account object `{"accounts": [{"label": "...", "entries": [...]}, ...]}`
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut accounts = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();

        let mut load_one = |p: &Path, accounts: &mut Vec<CookieAccount>| {
            let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
            if !seen.insert(canon) {
                return;
            }
            match Self::load_file(p) {
                Ok(mut got) => accounts.append(&mut got),
                Err(e) => warn!("failed to load {}: {}", p.display(), e),
            }
        };

        if path.exists() {
            load_one(path, &mut accounts);
        } else {
            warn!("{} not found", path.display());
        }

        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        match std::fs::read_dir(&parent) {
            Ok(rd) => {
                let mut sibs: Vec<PathBuf> = rd
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        if !p.is_file() {
                            return false;
                        }
                        let n = match p.file_name().and_then(|s| s.to_str()) {
                            Some(n) => n,
                            None => return false,
                        };
                        n.starts_with("cookies")
                            && n.ends_with(".json")
                            && n != "cookies.example.json"
                    })
                    .collect();
                sibs.sort();
                for p in sibs {
                    load_one(&p, &mut accounts);
                }
            }
            Err(e) => warn!(
                "could not scan {} for cookie files: {}",
                parent.display(),
                e
            ),
        }

        // Sidecar useragents.json: { "alice": "UA-string", ... }. Lets users
        // attach a per-account UA without editing the Cookie-Editor JSON.
        // Only fills accounts that don't already carry a nested user_agent.
        let ua_map = load_useragents(&parent);
        if !ua_map.is_empty() {
            for acc in accounts.iter_mut() {
                if acc.user_agent.is_none() {
                    if let Some(ua) = ua_map.get(&acc.label) {
                        acc.user_agent = Some(ua.clone());
                    }
                }
            }
        }

        for acc in &accounts {
            info!(
                "loaded {} cookies for account '{}'",
                acc.entries.len(),
                acc.label
            );
            if acc.any_expired() {
                info!(
                    "account '{}' has stale cookie expiration timestamps; live account check decides usability",
                    acc.label
                );
            }
        }

        if accounts.is_empty() {
            warn!("no cookies loaded, non incognito-viewable posts will NOT work");
        }

        let last_failures = (0..accounts.len()).map(|_| AtomicU64::new(0)).collect();
        let consecutive_failures = (0..accounts.len()).map(|_| AtomicU64::new(0)).collect();
        Ok(Self {
            accounts,
            last_failures,
            consecutive_failures,
            affinity: Mutex::new(HashMap::new()),
        })
    }

    fn load_file(path: &Path) -> anyhow::Result<Vec<CookieAccount>> {
        let raw = std::fs::read_to_string(path)?;
        let v: serde_json::Value = serde_json::from_str(&raw)?;

        let fname_label = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| {
                let t = s.strip_prefix("cookies").unwrap_or(s);
                t.trim_start_matches(['-', '_']).to_string()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "default".into());

        match v {
            serde_json::Value::Array(_) => {
                let entries: Vec<CookieEntry> = serde_json::from_value(v)?;
                if entries.is_empty() {
                    Ok(Vec::new())
                } else {
                    Ok(vec![CookieAccount::new(fname_label, entries, None)])
                }
            }
            serde_json::Value::Object(_) => {
                #[derive(Deserialize)]
                struct AccountIn {
                    label: String,
                    entries: Vec<CookieEntry>,
                    #[serde(default, rename = "user_agent", alias = "userAgent")]
                    user_agent: Option<String>,
                }
                let raw_accs = v.get("accounts").cloned().unwrap_or_default();
                let arr: Vec<AccountIn> = serde_json::from_value(raw_accs)?;
                Ok(arr
                    .into_iter()
                    .map(|a| CookieAccount::new(a.label, a.entries, a.user_agent))
                    .collect())
            }
            _ => anyhow::bail!("unsupported cookies file shape"),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }
}

/// Read an optional `useragents.json` sidecar from `dir`. Shape:
/// `{ "<account-label>": "<user-agent>", ... }`. Missing file => empty map;
/// parse errors are warned but non-fatal so a typo doesn't take the server
/// down.
fn load_useragents(dir: &Path) -> HashMap<String, String> {
    let path = dir.join("useragents.json");
    if !path.exists() {
        return HashMap::new();
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            warn!("could not read {}: {}", path.display(), e);
            return HashMap::new();
        }
    };
    match serde_json::from_str::<HashMap<String, String>>(&raw) {
        Ok(m) => {
            info!(
                "loaded {} user-agent overrides from {}",
                m.len(),
                path.display()
            );
            m
        }
        Err(e) => {
            warn!("failed to parse {}: {}", path.display(), e);
            HashMap::new()
        }
    }
}

impl CookieJar {
    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    /// Pick a specific account by index (modulo account count). Returns None for empty jar.
    pub fn account_at(&self, i: usize) -> Option<&CookieAccount> {
        if self.accounts.is_empty() {
            return None;
        }
        Some(&self.accounts[i % self.accounts.len()])
    }

    /// Mark the account at `i` (mod len) as having just failed. Future
    /// requests skip it on first attempt for [`ACCOUNT_COOLDOWN_SECS`].
    ///
    /// Returns the new count of consecutive failures since the last
    /// success. Callers can use this to fire admin notifications at a
    /// chosen threshold (see [`NOTIFY_FAILURE_THRESHOLD`]).
    pub fn mark_failed(&self, i: usize) -> u64 {
        if self.accounts.is_empty() {
            return 0;
        }
        let idx = i % self.accounts.len();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.last_failures[idx].store(now, Ordering::Relaxed);
        self.consecutive_failures[idx].fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Mark the account at `i` (mod len) as healthy — clears the cooldown
    /// and resets the consecutive-failure counter.
    pub fn mark_ok(&self, i: usize) {
        if self.accounts.is_empty() {
            return;
        }
        let idx = i % self.accounts.len();
        self.last_failures[idx].store(0, Ordering::Relaxed);
        self.consecutive_failures[idx].store(0, Ordering::Relaxed);
    }

    /// Reset the consecutive-failure counter without clearing cooldown.
    /// Used after firing an admin notification, so we re-alert if the
    /// account fails another N times rather than spamming every request.
    pub fn reset_failure_count(&self, i: usize) {
        if self.accounts.is_empty() {
            return;
        }
        self.consecutive_failures[i % self.accounts.len()].store(0, Ordering::Relaxed);
    }

    /// True if the account at `i` is currently in cooldown.
    pub fn in_cooldown(&self, i: usize) -> bool {
        if self.accounts.is_empty() {
            return false;
        }
        let last = self.last_failures[i % self.accounts.len()].load(Ordering::Relaxed);
        if last == 0 {
            return false;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_sub(last) < ACCOUNT_COOLDOWN_SECS
    }

    /// Label of the account at `i` (for logging).
    pub fn label_at(&self, i: usize) -> Option<&str> {
        if self.accounts.is_empty() {
            return None;
        }
        Some(&self.accounts[i % self.accounts.len()].label)
    }

    /// Account index previously known to succeed for this scope key.
    pub fn affinity_for(&self, key: &str) -> Option<usize> {
        self.affinity.lock().ok()?.get(key).copied()
    }

    /// Record `account_idx` as the preferred account for `key`. Bounded by
    /// [`AFFINITY_CAP`] — past that, an arbitrary entry is evicted.
    pub fn set_affinity(&self, key: String, account_idx: usize) {
        if self.accounts.is_empty() {
            return;
        }
        let Ok(mut m) = self.affinity.lock() else {
            return;
        };
        if m.len() >= AFFINITY_CAP && !m.contains_key(&key) {
            if let Some(k) = m.keys().next().cloned() {
                m.remove(&k);
            }
        }
        m.insert(key, account_idx % self.accounts.len());
    }

    /// Forget the affinity mapping for `key`. Called when the pinned
    /// account fails — we'd rather re-discover a working one than keep
    /// paying the slow first-try cost.
    pub fn forget_affinity(&self, key: &str) {
        if let Ok(mut m) = self.affinity.lock() {
            m.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn auto_discovers_sibling_cookie_files() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        let alice = dir.path().join("cookies-alice.json");
        let two = dir.path().join("cookies2.json");
        let example = dir.path().join("cookies.example.json");

        fs::write(&main, r#"[{"name":"c_user","value":"1"}]"#).unwrap();
        fs::write(&alice, r#"[{"name":"c_user","value":"2"}]"#).unwrap();
        fs::write(&two, r#"[{"name":"c_user","value":"3"}]"#).unwrap();
        fs::write(&example, r#"[{"name":"c_user","value":"99"}]"#).unwrap();

        let jar = CookieJar::load(&main).unwrap();
        let mut labels: Vec<_> = jar.accounts.iter().map(|a| a.label.clone()).collect();
        labels.sort();
        assert_eq!(labels, vec!["2", "alice", "default"]);
    }

    #[test]
    fn missing_primary_still_picks_up_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        let alice = dir.path().join("cookies-alice.json");
        fs::write(&alice, r#"[{"name":"c_user","value":"2"}]"#).unwrap();

        let jar = CookieJar::load(&main).unwrap();
        let labels: Vec<_> = jar.accounts.iter().map(|a| a.label.clone()).collect();
        assert_eq!(labels, vec!["alice"]);
    }

    #[test]
    fn affinity_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        fs::write(
            &main,
            r#"{"accounts":[{"label":"x","entries":[{"name":"c_user","value":"1"}]},{"label":"y","entries":[{"name":"c_user","value":"2"}]}]}"#,
        ).unwrap();
        let jar = CookieJar::load(&main).unwrap();
        assert_eq!(jar.affinity_for("groups/123"), None);
        jar.set_affinity("groups/123".into(), 1);
        assert_eq!(jar.affinity_for("groups/123"), Some(1));
        jar.forget_affinity("groups/123");
        assert_eq!(jar.affinity_for("groups/123"), None);
    }

    #[test]
    fn affinity_index_clamped_to_account_count() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        fs::write(&main, r#"[{"name":"c_user","value":"1"}]"#).unwrap();
        let jar = CookieJar::load(&main).unwrap();
        jar.set_affinity("user/alice".into(), 42);
        assert_eq!(jar.affinity_for("user/alice"), Some(0));
    }

    #[test]
    fn affinity_noop_on_empty_jar() {
        let jar = CookieJar::empty();
        jar.set_affinity("user/alice".into(), 0);
        assert_eq!(jar.affinity_for("user/alice"), None);
    }

    #[test]
    fn multi_account_object_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        fs::write(
            &main,
            r#"{"accounts":[{"label":"x","entries":[{"name":"c_user","value":"1"}]},{"label":"y","entries":[{"name":"c_user","value":"2"}]}]}"#,
        ).unwrap();
        let jar = CookieJar::load(&main).unwrap();
        let labels: Vec<_> = jar.accounts.iter().map(|a| a.label.clone()).collect();
        assert_eq!(labels, vec!["x", "y"]);
    }

    #[test]
    fn nested_user_agent_parsed_per_account() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        fs::write(
            &main,
            r#"{"accounts":[
                {"label":"x","user_agent":"UA-X","entries":[{"name":"c_user","value":"1"}]},
                {"label":"y","entries":[{"name":"c_user","value":"2"}]}
            ]}"#,
        )
        .unwrap();
        let jar = CookieJar::load(&main).unwrap();
        assert_eq!(jar.accounts[0].user_agent.as_deref(), Some("UA-X"));
        assert_eq!(jar.accounts[1].user_agent, None);
    }

    #[test]
    fn flat_array_has_no_user_agent_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        fs::write(&main, r#"[{"name":"c_user","value":"1"}]"#).unwrap();
        let jar = CookieJar::load(&main).unwrap();
        assert_eq!(jar.accounts[0].user_agent, None);
    }

    #[test]
    fn useragents_sidecar_fills_flat_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        let alice = dir.path().join("cookies-alice.json");
        let ua = dir.path().join("useragents.json");
        fs::write(&main, r#"[{"name":"c_user","value":"1"}]"#).unwrap();
        fs::write(&alice, r#"[{"name":"c_user","value":"2"}]"#).unwrap();
        fs::write(&ua, r#"{"default":"UA-DEFAULT","alice":"UA-ALICE"}"#).unwrap();
        let jar = CookieJar::load(&main).unwrap();
        let by_label: std::collections::HashMap<_, _> = jar
            .accounts
            .iter()
            .map(|a| (a.label.clone(), a.user_agent.clone()))
            .collect();
        assert_eq!(by_label["default"].as_deref(), Some("UA-DEFAULT"));
        assert_eq!(by_label["alice"].as_deref(), Some("UA-ALICE"));
    }

    #[test]
    fn sidecar_does_not_override_nested_user_agent() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        let ua = dir.path().join("useragents.json");
        fs::write(
            &main,
            r#"{"accounts":[{"label":"x","user_agent":"UA-NESTED","entries":[{"name":"c_user","value":"1"}]}]}"#,
        ).unwrap();
        fs::write(&ua, r#"{"x":"UA-SIDECAR"}"#).unwrap();
        let jar = CookieJar::load(&main).unwrap();
        assert_eq!(jar.accounts[0].user_agent.as_deref(), Some("UA-NESTED"));
    }

    #[test]
    fn missing_useragents_file_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        fs::write(&main, r#"[{"name":"c_user","value":"1"}]"#).unwrap();
        let jar = CookieJar::load(&main).unwrap();
        assert_eq!(jar.accounts[0].user_agent, None);
    }

    #[test]
    fn malformed_useragents_file_is_warn_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        let ua = dir.path().join("useragents.json");
        fs::write(&main, r#"[{"name":"c_user","value":"1"}]"#).unwrap();
        fs::write(&ua, "not json {{{").unwrap();
        let jar = CookieJar::load(&main).unwrap();
        assert_eq!(jar.accounts[0].user_agent, None);
    }

    #[test]
    fn mark_failed_returns_consecutive_count() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        fs::write(&main, r#"[{"name":"c_user","value":"1"}]"#).unwrap();
        let jar = CookieJar::load(&main).unwrap();
        assert_eq!(jar.mark_failed(0), 1);
        assert_eq!(jar.mark_failed(0), 2);
        assert_eq!(jar.mark_failed(0), 3);
        jar.mark_ok(0);
        assert_eq!(jar.mark_failed(0), 1);
    }

    #[test]
    fn reset_failure_count_does_not_clear_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        fs::write(&main, r#"[{"name":"c_user","value":"1"}]"#).unwrap();
        let jar = CookieJar::load(&main).unwrap();
        jar.mark_failed(0);
        jar.mark_failed(0);
        assert!(jar.in_cooldown(0));
        jar.reset_failure_count(0);
        assert!(jar.in_cooldown(0));
        assert_eq!(jar.mark_failed(0), 1);
    }
}
