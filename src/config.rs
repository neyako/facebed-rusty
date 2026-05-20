use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub timezone: i32,
    pub banned_users: Vec<String>,
    pub notifier_webhook: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 9812,
            timezone: 7,
            banned_users: Vec::new(),
            notifier_webhook: String::new(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let cfg: Self = serde_yaml::from_str(&raw)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.timezone < -12 || self.timezone > 14 {
            anyhow::bail!("invalid timezone offset: {}", self.timezone);
        }
        Ok(())
    }
}
