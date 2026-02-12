use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct HimalayaConfig {
    pub accounts: HashMap<String, Account>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct Account {
    #[serde(default)]
    pub default: bool,
    pub email: String,
    #[serde(rename = "display-name")]
    pub display_name: Option<String>,
    pub backend: Backend,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct Backend {
    #[serde(rename = "type")]
    pub backend_type: String,
    pub host: String,
    pub port: u16,
    pub login: String,
    pub encryption: Option<Encryption>,
    pub auth: Auth,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct Encryption {
    #[serde(rename = "type")]
    pub encryption_type: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct Auth {
    #[serde(rename = "type")]
    pub auth_type: String,
    pub raw: Option<String>,
    pub command: Option<String>,
}

impl HimalayaConfig {
    pub fn load() -> Result<Self> {
        let config_path = config_path()?;
        let content = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read config: {}", config_path.display()))?;
        let config: HimalayaConfig =
            toml::from_str(&content).context("Failed to parse himalaya config")?;
        Ok(config)
    }

    pub fn default_account(&self) -> Result<(&str, &Account)> {
        self.accounts
            .iter()
            .find(|(_, acc)| acc.default)
            .or_else(|| self.accounts.iter().next())
            .map(|(name, acc)| (name.as_str(), acc))
            .context("No accounts found in himalaya config")
    }
}

fn config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let path = PathBuf::from(home)
        .join(".config")
        .join("himalaya")
        .join("config.toml");
    Ok(path)
}
