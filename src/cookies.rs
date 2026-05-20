use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

/// One cookie entry as exported by Cookie-Editor.
#[derive(Debug, Clone, Deserialize)]
pub struct CookieEntry {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default, rename = "expirationDate")]
    pub expiration_date: Option<f64>,
}

/// A named set of cookies = one Facebook account.
#[derive(Debug, Clone)]
pub struct CookieAccount {
    pub label: String,
    pub entries: Vec<CookieEntry>,
}

impl CookieAccount {
    pub fn header_value(&self) -> String {
        self.entries
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn any_expired(&self) -> bool {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as f64).unwrap_or(0.0);
        self.entries.iter().any(|c| c.expiration_date.map(|e| e <= now).unwrap_or(false))
    }
}

/// Pool of accounts. Empty pool = anonymous fetches.
#[derive(Debug)]
pub struct CookieJar {
    accounts: Vec<CookieAccount>,
    cursor: AtomicUsize,
}

impl CookieJar {
    pub fn empty() -> Self {
        Self { accounts: Vec::new(), cursor: AtomicUsize::new(0) }
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
            Err(e) => warn!("could not scan {} for cookie files: {}", parent.display(), e),
        }

        for acc in &accounts {
            info!("loaded {} cookies for account '{}'", acc.entries.len(), acc.label);
            if acc.any_expired() {
                warn!("account '{}' has expired cookies", acc.label);
            }
        }

        if accounts.is_empty() {
            warn!("no cookies loaded, non incognito-viewable posts will NOT work");
        }

        Ok(Self { accounts, cursor: AtomicUsize::new(0) })
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
                    Ok(vec![CookieAccount { label: fname_label, entries }])
                }
            }
            serde_json::Value::Object(_) => {
                #[derive(Deserialize)]
                struct AccountIn {
                    label: String,
                    entries: Vec<CookieEntry>,
                }
                let raw_accs = v.get("accounts").cloned().unwrap_or_default();
                let arr: Vec<AccountIn> = serde_json::from_value(raw_accs)?;
                Ok(arr
                    .into_iter()
                    .map(|a| CookieAccount { label: a.label, entries: a.entries })
                    .collect())
            }
            _ => anyhow::bail!("unsupported cookies file shape"),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    /// Round-robin select an account.
    pub fn next_account(&self) -> Option<&CookieAccount> {
        if self.accounts.is_empty() {
            return None;
        }
        let i = self.cursor.fetch_add(1, Ordering::Relaxed) % self.accounts.len();
        Some(&self.accounts[i])
    }

    /// Pick a specific account by index (modulo account count). Returns None for empty jar.
    pub fn account_at(&self, i: usize) -> Option<&CookieAccount> {
        if self.accounts.is_empty() {
            return None;
        }
        Some(&self.accounts[i % self.accounts.len()])
    }

    /// Current round-robin cursor (without advancing). Used to seed retry loops so the
    /// first attempt matches the regular round-robin pick.
    pub fn cursor(&self) -> usize {
        self.cursor.load(Ordering::Relaxed)
    }

    /// Advance the round-robin cursor by one.
    pub fn advance_cursor(&self) {
        self.cursor.fetch_add(1, Ordering::Relaxed);
    }

    pub fn expired_labels(&self) -> Vec<String> {
        self.accounts.iter().filter(|a| a.any_expired()).map(|a| a.label.clone()).collect()
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
}
