use serde::Deserialize;
use std::path::Path;
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

    /// Load cookies. Accepts either:
    ///   - Cookie-Editor flat array: `[{name, value, ...}, ...]` → 1 account labeled "default"
    ///   - Multi-account object: `{"accounts": [{"label": "...", "entries": [...]}, ...]}`
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            warn!("{} not found, non incognito-viewable posts will NOT work", path.display());
            return Ok(Self::empty());
        }

        let raw = std::fs::read_to_string(path)?;
        let v: serde_json::Value = serde_json::from_str(&raw)?;

        let accounts = match v {
            serde_json::Value::Array(_) => {
                let entries: Vec<CookieEntry> = serde_json::from_value(v)?;
                vec![CookieAccount { label: "default".into(), entries }]
            }
            serde_json::Value::Object(_) => {
                #[derive(Deserialize)]
                struct AccountIn {
                    label: String,
                    entries: Vec<CookieEntry>,
                }
                let raw_accs = v.get("accounts").cloned().unwrap_or_default();
                let arr: Vec<AccountIn> = serde_json::from_value(raw_accs)?;
                arr.into_iter().map(|a| CookieAccount { label: a.label, entries: a.entries }).collect()
            }
            _ => anyhow::bail!("unsupported cookies.json shape"),
        };

        for acc in &accounts {
            info!("loaded {} cookies for account '{}'", acc.entries.len(), acc.label);
            if acc.any_expired() {
                warn!("account '{}' has expired cookies", acc.label);
            }
        }

        Ok(Self { accounts, cursor: AtomicUsize::new(0) })
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// Round-robin select an account.
    pub fn next_account(&self) -> Option<&CookieAccount> {
        if self.accounts.is_empty() {
            return None;
        }
        let i = self.cursor.fetch_add(1, Ordering::Relaxed) % self.accounts.len();
        Some(&self.accounts[i])
    }

    pub fn expired_labels(&self) -> Vec<String> {
        self.accounts.iter().filter(|a| a.any_expired()).map(|a| a.label.clone()).collect()
    }
}
