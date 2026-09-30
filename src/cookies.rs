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

/// Cooldown after a soft rate limit (HTTP 429/503). Short: FB rate limits are
/// usually per-minute and the cookie itself is healthy. A `Retry-After` value
/// overrides this, capped at the max below.
pub const RATE_LIMIT_COOLDOWN_SECS: u64 = 60;
pub const RATE_LIMIT_COOLDOWN_MAX_SECS: u64 = 600;

/// Cooldown after a checkpoint / account-recovery redirect. Long: a checkpoint
/// will not clear within minutes; it needs a human to re-export the cookie.
pub const CHECKPOINT_COOLDOWN_SECS: u64 = 1800;

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
    /// Per-account unix seconds when cooldown ends. Parallel to `accounts`.
    /// Zero means "not in cooldown".
    cooldown_until: Vec<AtomicU64>,
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
            cooldown_until: Vec::new(),
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
    ///     (also accepted wrapped as `{"url": ..., "cookies": [...]}`)
    ///   - Multi-account object `{"accounts": [{"label": "...", "entries": [...]}, ...]}`
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        Self::load_with(path, false)
    }

    /// Like [`Self::load`], but an unreadable cookie directory or an
    /// unreadable or malformed cookie file is an error instead of a skipped warning. SIGHUP reload uses this so a bad
    /// edit keeps the running jar instead of silently dropping accounts.
    pub fn load_strict(path: &Path) -> anyhow::Result<Self> {
        Self::load_with(path, true)
    }

    fn load_with(path: &Path, strict: bool) -> anyhow::Result<Self> {
        if !path.exists() {
            warn!("{} not found", path.display());
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let files = match cookie_files(path, &parent) {
            Ok(files) => files,
            Err(e) if strict => anyhow::bail!("could not scan {}: {e}", parent.display()),
            Err(e) => {
                warn!("could not scan {} for cookie files: {e}", parent.display());
                path.exists()
                    .then(|| path.to_path_buf())
                    .into_iter()
                    .collect()
            }
        };
        let mut accounts = Vec::new();
        for file in files {
            match Self::load_file(&file) {
                Ok(mut got) => accounts.append(&mut got),
                Err(e) if strict => anyhow::bail!("{}: {e}", file.display()),
                Err(e) => warn!("failed to load {}: {}", file.display(), e),
            }
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

        let cooldown_until = (0..accounts.len()).map(|_| AtomicU64::new(0)).collect();
        let consecutive_failures = (0..accounts.len()).map(|_| AtomicU64::new(0)).collect();
        Ok(Self {
            accounts,
            cooldown_until,
            consecutive_failures,
            affinity: Mutex::new(HashMap::new()),
        })
    }

    fn load_file(path: &Path) -> anyhow::Result<Vec<CookieAccount>> {
        let raw = std::fs::read_to_string(path)?;
        let v: serde_json::Value = serde_json::from_str(&raw)?;
        // `{"url": ..., "cookies": [...]}` exports wrap the flat array.
        let v = match v {
            serde_json::Value::Object(mut map) if map.contains_key("cookies") => {
                map.remove("cookies").unwrap_or_default()
            }
            v => v,
        };

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

/// `path` (if present) plus sibling `cookies*.json` files in `parent`,
/// primary first, siblings sorted, each file once.
fn cookie_files(path: &Path, parent: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if path.exists() {
        files.push(path.to_path_buf());
    }
    let mut siblings: Vec<PathBuf> = std::fs::read_dir(parent)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name().and_then(|s| s.to_str()).is_some_and(|n| {
                    n.starts_with("cookies") && n.ends_with(".json") && n != "cookies.example.json"
                })
        })
        .collect();
    siblings.sort();
    files.extend(siblings);
    let mut seen: HashSet<PathBuf> = HashSet::new();
    files.retain(|p| seen.insert(std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())));
    Ok(files)
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

    fn set_cooldown(&self, idx: usize, secs: u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.cooldown_until[idx].store(now.saturating_add(secs), Ordering::Relaxed);
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
        self.set_cooldown(idx, ACCOUNT_COOLDOWN_SECS);
        self.consecutive_failures[idx].fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Soft rate limit: cool down briefly and do NOT touch the consecutive
    /// failure counter. A rate limit is not a bad cookie.
    pub fn mark_rate_limited(&self, i: usize, retry_after: Option<u64>) {
        if self.accounts.is_empty() {
            return;
        }
        let idx = i % self.accounts.len();
        let secs = retry_after
            .map(|s| s.min(RATE_LIMIT_COOLDOWN_MAX_SECS))
            .unwrap_or(RATE_LIMIT_COOLDOWN_SECS);
        self.set_cooldown(idx, secs);
    }

    /// Checkpoint / recovery block: long cooldown, and count it as a failure so
    /// the admin alert fires when the bad-account threshold is reached.
    pub fn mark_checkpointed(&self, i: usize) -> u64 {
        if self.accounts.is_empty() {
            return 0;
        }
        let idx = i % self.accounts.len();
        self.set_cooldown(idx, CHECKPOINT_COOLDOWN_SECS);
        self.consecutive_failures[idx].fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Mark the account at `i` (mod len) as healthy — clears the cooldown
    /// and resets the consecutive-failure counter.
    pub fn mark_ok(&self, i: usize) {
        if self.accounts.is_empty() {
            return;
        }
        let idx = i % self.accounts.len();
        self.cooldown_until[idx].store(0, Ordering::Relaxed);
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
        let until = self.cooldown_until[i % self.accounts.len()].load(Ordering::Relaxed);
        if until == 0 {
            return false;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now < until
    }

    /// Label of the account at `i` (for logging).
    pub fn label_at(&self, i: usize) -> Option<&str> {
        if self.accounts.is_empty() {
            return None;
        }
        Some(&self.accounts[i % self.accounts.len()].label)
    }

    /// Carry runtime state from the jar a SIGHUP reload replaces. Affinity
    /// follows the label (same Facebook account). Cooldown and failure counts
    /// carry over only if the cookie is unchanged: a re-exported cookie is
    /// usually the fix for them, so it starts clean.
    pub fn inherit_state(&self, old: &CookieJar) {
        let index_of = |label: &str| self.accounts.iter().position(|a| a.label == label);
        for (j, previous) in old.accounts.iter().enumerate() {
            let Some(i) = index_of(&previous.label) else {
                continue;
            };
            if previous.header_value() == self.accounts[i].header_value() {
                let until = old.cooldown_until[j].load(Ordering::Relaxed);
                let failures = old.consecutive_failures[j].load(Ordering::Relaxed);
                self.cooldown_until[i].store(until, Ordering::Relaxed);
                self.consecutive_failures[i].store(failures, Ordering::Relaxed);
            }
        }
        let (Ok(previous), Ok(mut current)) = (old.affinity.lock(), self.affinity.lock()) else {
            return;
        };
        for (key, &j) in previous.iter() {
            if let Some(i) = old.accounts.get(j).and_then(|a| index_of(&a.label)) {
                current.insert(key.clone(), i);
            }
        }
    }

    /// Account indices in configured priority, healthy ones first and
    /// cooled-down ones as a last resort.
    pub fn priority_order(&self) -> Vec<usize> {
        let (healthy, cooled): (Vec<usize>, Vec<usize>) =
            (0..self.len()).partition(|&i| !self.in_cooldown(i));
        healthy.into_iter().chain(cooled).collect()
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
        fs::write(
            &two,
            r#"{"url":"https://www.facebook.com","cookies":[{"name":"c_user","value":"3"}]}"#,
        )
        .unwrap();
        fs::write(&example, r#"[{"name":"c_user","value":"99"}]"#).unwrap();

        let jar = CookieJar::load(&main).unwrap();
        let mut labels: Vec<_> = jar.accounts.iter().map(|a| a.label.clone()).collect();
        labels.sort();
        assert_eq!(labels, vec!["2", "alice", "default"]);
    }

    #[test]
    fn strict_load_rejects_a_malformed_sibling_lenient_skips_it() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        fs::write(&main, r#"[{"name":"c_user","value":"1"}]"#).unwrap();
        fs::write(dir.path().join("cookies-bad.json"), "{not json").unwrap();
        assert_eq!(CookieJar::load(&main).unwrap().len(), 1);
        assert!(CookieJar::load_strict(&main).is_err());
    }

    #[test]
    fn strict_load_rejects_an_unreadable_cookie_dir() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("gone").join("cookies.json");
        assert!(CookieJar::load(&main).unwrap().is_empty());
        assert!(CookieJar::load_strict(&main).is_err());
    }

    #[test]
    fn reload_keeps_affinity_and_only_keeps_health_for_unchanged_cookies() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        let alice = dir.path().join("cookies-alice.json");
        fs::write(&main, r#"[{"name":"c_user","value":"1"}]"#).unwrap();
        fs::write(&alice, r#"[{"name":"c_user","value":"2"}]"#).unwrap();
        let old = CookieJar::load(&main).unwrap();
        let (default, alice_index) = (0, 1);
        old.mark_failed(default);
        old.mark_failed(alice_index);
        old.set_affinity("groups/1".into(), alice_index);

        // alice re-exported her cookie; default is untouched.
        fs::write(&alice, r#"[{"name":"c_user","value":"3"}]"#).unwrap();
        let new = CookieJar::load(&main).unwrap();
        new.inherit_state(&old);
        assert!(new.in_cooldown(default));
        assert!(!new.in_cooldown(alice_index));
        assert_eq!(new.affinity_for("groups/1"), Some(alice_index));
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
    fn rate_limit_cools_down_without_marking_bad() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        fs::write(&main, r#"[{"name":"c_user","value":"1"}]"#).unwrap();
        let jar = CookieJar::load(&main).unwrap();

        jar.mark_rate_limited(0, None);
        assert!(jar.in_cooldown(0));
        assert_eq!(jar.mark_failed(0), 1);
    }

    #[test]
    fn checkpoint_cools_down_and_counts_failures() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("cookies.json");
        fs::write(&main, r#"[{"name":"c_user","value":"1"}]"#).unwrap();
        let jar = CookieJar::load(&main).unwrap();

        assert_eq!(jar.mark_checkpointed(0), 1);
        assert!(jar.in_cooldown(0));
        assert_eq!(jar.mark_checkpointed(0), 2);
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
