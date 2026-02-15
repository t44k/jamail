use anyhow::{Context, Result};
use indexmap::IndexMap;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct JamailConfig {
    pub accounts: IndexMap<String, JamailAccount>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct JamailAccount {
    #[serde(default)]
    pub default: bool,
    pub email: String,
    pub display_name: Option<String>,
    pub imap: ImapConfig,
    pub folders: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub login: String,
    pub auth: AuthConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AuthConfig {
    #[serde(rename = "type")]
    pub auth_type: String,
    pub value: String,
}

impl AuthConfig {
    /// Resolve the password: if auth_type is "command", run value as a shell
    /// command and capture stdout. Otherwise return value directly.
    pub fn resolve_password(&self) -> Result<String> {
        match self.auth_type.as_str() {
            "command" => {
                let output = std::process::Command::new("sh")
                    .args(["-c", &self.value])
                    .output()
                    .with_context(|| format!("Failed to run auth command: {}", self.value))?;
                if !output.status.success() {
                    anyhow::bail!(
                        "Auth command failed (exit {}): {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            }
            _ => Ok(self.value.clone()),
        }
    }
}

impl JamailConfig {
    pub fn load() -> Result<Self> {
        let config_path = config_path()?;
        let content = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read config: {}", config_path.display()))?;
        let config: JamailConfig =
            serde_yml::from_str(&content).context("Failed to parse jamail config")?;
        Ok(config)
    }

    pub fn default_account(&self) -> Result<(&str, &JamailAccount)> {
        self.accounts
            .iter()
            .find(|(_, acc)| acc.default)
            .or_else(|| self.accounts.iter().next())
            .map(|(name, acc)| (name.as_str(), acc))
            .context("No accounts found in jamail config")
    }
}

fn config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let path = PathBuf::from(home)
        .join(".config")
        .join("jamail")
        .join("config.yaml");
    Ok(path)
}
