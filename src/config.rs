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
    pub smtp: Option<SmtpConfig>,
    /// Ordered list of folders to sync and to show in the folder selector.
    /// Unset: all server folders are synced/shown (deterministic default
    /// order: INBOX first, then alphabetical).
    pub folders: Option<Vec<String>>,
    /// Additional "From" identities selectable at compose time (cycled with
    /// Left/Right on the From field). Unset: falls back to a single identity
    /// built from `display_name`/`email`.
    pub senders: Option<Vec<String>>,
    /// IMAP folder to upload a copy of successfully sent messages to.
    /// Unset (default): sent messages are kept in the local cache only —
    /// nothing is uploaded to the server.
    pub sent_folder: Option<String>,
    /// IMAP folder to upload newly-created drafts to (on first explicit
    /// save). Unset (default): drafts are kept in the local cache only —
    /// nothing is uploaded to the server.
    pub draft_folder: Option<String>,
    /// Folders that trigger a desktop notification + sound when new mail
    /// arrives. Unset (default): no notifications for this account.
    pub notify_folders: Option<Vec<String>>,
    pub color: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub login: String,
    pub auth: AuthConfig,
    #[serde(default = "default_true")]
    pub starttls: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> JamailConfig {
        serde_yml::from_str(yaml).expect("valid jamail config")
    }

    #[test]
    fn minimal_legacy_config_parses_with_all_new_fields_defaulting_to_none() {
        // A config written before senders/folders/sent_folder/draft_folder/
        // notify_folders existed must keep parsing without changes.
        let cfg = parse(
            "accounts:\n\
             \x20 personal:\n\
             \x20   email: alice@example.com\n\
             \x20   imap:\n\
             \x20     host: imap.example.com\n\
             \x20     port: 993\n\
             \x20     login: alice@example.com\n\
             \x20     auth:\n\
             \x20       type: password\n\
             \x20       value: secret\n",
        );
        let (_, acc) = cfg.default_account().unwrap();
        assert!(acc.senders.is_none());
        assert!(acc.folders.is_none());
        assert!(acc.sent_folder.is_none());
        assert!(acc.draft_folder.is_none());
        assert!(acc.notify_folders.is_none());
        assert!(acc.smtp.is_none());
        assert!(!acc.default);
    }

    #[test]
    fn multiple_senders_parse_in_declared_order() {
        let cfg = parse(
            "accounts:\n\
             \x20 work:\n\
             \x20   email: alice@corp.com\n\
             \x20   senders:\n\
             \x20     - Alice Smith <alice@corp.com>\n\
             \x20     - Alice (Support) <support@corp.com>\n\
             \x20   imap:\n\
             \x20     host: imap.corp.com\n\
             \x20     port: 993\n\
             \x20     login: alice@corp.com\n\
             \x20     auth: {type: password, value: secret}\n",
        );
        let (_, acc) = cfg.default_account().unwrap();
        let senders = acc.senders.as_ref().expect("senders present");
        assert_eq!(
            senders,
            &vec![
                "Alice Smith <alice@corp.com>".to_string(),
                "Alice (Support) <support@corp.com>".to_string(),
            ]
        );
    }

    #[test]
    fn sent_and_draft_folders_are_independently_configurable() {
        let cfg = parse(
            "accounts:\n\
             \x20 work:\n\
             \x20   email: alice@corp.com\n\
             \x20   sent_folder: Sent\n\
             \x20   imap:\n\
             \x20     host: imap.corp.com\n\
             \x20     port: 993\n\
             \x20     login: alice@corp.com\n\
             \x20     auth: {type: password, value: secret}\n",
        );
        let (_, acc) = cfg.default_account().unwrap();
        assert_eq!(acc.sent_folder.as_deref(), Some("Sent"));
        // draft_folder was not set, and must stay independently disabled.
        assert!(acc.draft_folder.is_none());
    }

    #[test]
    fn notify_folders_parses_as_ordered_list() {
        let cfg = parse(
            "accounts:\n\
             \x20 personal:\n\
             \x20   email: alice@example.com\n\
             \x20   notify_folders: [INBOX, Important]\n\
             \x20   imap:\n\
             \x20     host: imap.example.com\n\
             \x20     port: 993\n\
             \x20     login: alice@example.com\n\
             \x20     auth: {type: password, value: secret}\n",
        );
        let (_, acc) = cfg.default_account().unwrap();
        assert_eq!(
            acc.notify_folders,
            Some(vec!["INBOX".to_string(), "Important".to_string()])
        );
    }

    #[test]
    fn auth_command_resolves_password_from_shell_output() {
        let auth = AuthConfig {
            auth_type: "command".to_string(),
            value: "echo -n hunter2".to_string(),
        };
        assert_eq!(auth.resolve_password().unwrap(), "hunter2");
    }

    #[test]
    fn auth_password_type_returns_value_literally() {
        let auth = AuthConfig {
            auth_type: "password".to_string(),
            value: "hunter2".to_string(),
        };
        assert_eq!(auth.resolve_password().unwrap(), "hunter2");
    }

    #[test]
    fn shipped_example_config_parses_and_documents_every_new_key() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.yaml");
        let content = std::fs::read_to_string(&path).expect("config.example.yaml is readable");
        let cfg = parse(&content);

        let work = &cfg.accounts["work"];
        assert!(work.senders.as_ref().is_some_and(|s| s.len() > 1));
        assert_eq!(work.sent_folder.as_deref(), Some("Sent"));
        assert_eq!(work.draft_folder.as_deref(), Some("Drafts"));
        assert_eq!(
            work.notify_folders.as_deref(),
            Some(["INBOX".to_string()].as_slice())
        );

        // The other example accounts must still parse with the new keys unset.
        let personal = &cfg.accounts["personal"];
        assert!(personal.senders.is_none());
        assert!(personal.sent_folder.is_none());
        assert!(personal.notify_folders.is_none());
    }
}
