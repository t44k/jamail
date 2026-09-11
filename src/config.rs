use anyhow::{Context, Result};
use indexmap::IndexMap;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct JamailConfig {
    pub accounts: IndexMap<String, JamailAccount>,
    /// Optional `jamaild`/`jamail` IPC settings. Unset (default): the socket
    /// path is derived from `$XDG_RUNTIME_DIR` (see `ipc::resolve_socket_path`).
    pub daemon: Option<DaemonConfig>,
    /// Optional `jacal` (calendar TUI) settings. Unset (default): every
    /// `JacalConfig` field falls back to its own default.
    pub jacal: Option<JacalConfig>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct JacalConfig {
    /// Which day a week starts on in `jacal`'s week/month/year grids.
    /// Unset (default): Monday.
    #[serde(default)]
    pub week_start: WeekStart,
}

/// Which day of the week `jacal`'s calendar grids start on.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum WeekStart {
    #[default]
    Monday,
    Sunday,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct DaemonConfig {
    /// Override the Unix domain socket path used for `jamaild`<->`jamail`
    /// IPC. Unset (default): `$XDG_RUNTIME_DIR/jamail/jamaild.sock`, falling
    /// back to `/tmp/jamail-<uid>/jamaild.sock` when `XDG_RUNTIME_DIR` isn't
    /// set. The `JAMAIL_SOCKET` environment variable, when set, always takes
    /// precedence over this setting on both binaries.
    pub socket_path: Option<String>,
    /// Optional: if set (e.g. `"127.0.0.1:5232"`), `jamaild` also binds
    /// this TCP address and serves a minimal, real inbound CalDAV (RFC
    /// 4791) HTTP server — see `caldav_server` module docs — exposing
    /// every account's `caldav`-configured local calendar cache to any
    /// CalDAV client (Basic auth against that account's own
    /// `caldav.login`/`caldav.auth`). Unset (default): no CalDAV server;
    /// `jamaild` starts exactly as before this existed.
    pub caldav_server_listen: Option<String>,
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
    /// When `folders` is set, also sync/show remote folders that aren't
    /// listed there, appended after the configured ones in deterministic
    /// order (INBOX-first, then alphabetical). Default `false`: only the
    /// folders explicitly listed in `folders` are synced/shown. Has no
    /// effect when `folders` is unset — everything is already shown/synced
    /// in that case.
    #[serde(default)]
    pub show_unlisted_folders: bool,
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
    /// Optional CalDAV configuration for this account. Unset (default): no
    /// calendar sync at all for this account — `jamaild` never spawns a
    /// calendar sync thread and `jacal` shows no calendars from it. See
    /// [`CalDavConfig`].
    pub caldav: Option<CalDavConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CalDavConfig {
    /// The CalDAV server URL. This can be either a full calendar-home-set
    /// URL (if you already know it) or any URL under the account's CalDAV
    /// root — [`crate::caldav::CalDavClient::discover_calendars`] will
    /// attempt the standard `current-user-principal` ->
    /// `calendar-home-set` discovery chain from here, falling back to
    /// treating this URL itself as the home set if that chain isn't
    /// supported by the server.
    pub url: String,
    pub login: String,
    pub auth: AuthConfig,
    /// Restrict sync to calendars whose *display name* is in this list
    /// (case-sensitive, matched against the server's `DAV:displayname`).
    /// Unset (default): sync every discovered calendar collection.
    pub calendars: Option<Vec<String>>,
    /// How often to poll the server for changes, in seconds. CalDAV has no
    /// push/IDLE equivalent this client implements, so sync is poll-based.
    /// Unset (default): [`default_caldav_poll_interval_secs`] (300 = 5
    /// minutes).
    #[serde(default = "default_caldav_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// Default alarm lead times (minutes before an event's start) applied
    /// to events that carry no `VALARM` of their own. Unset (default): no
    /// synthetic alarm — only events with an explicit `VALARM` produce a
    /// notification. Each entry fires its own notification (e.g. `[30,
    /// 5]` notifies both 30 and 5 minutes before).
    pub default_alarm_minutes_before: Option<Vec<i64>>,
}

fn default_caldav_poll_interval_secs() -> u64 {
    300
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
        assert!(!acc.show_unlisted_folders);
    }

    #[test]
    fn show_unlisted_folders_defaults_to_false_when_folders_is_configured() {
        // A config that sets `folders` but doesn't mention the new key must
        // still default to false (only the listed folders are synced/shown).
        let cfg = parse(
            "accounts:\n\
             \x20 work:\n\
             \x20   email: alice@corp.com\n\
             \x20   folders: [INBOX, Sent]\n\
             \x20   imap:\n\
             \x20     host: imap.corp.com\n\
             \x20     port: 993\n\
             \x20     login: alice@corp.com\n\
             \x20     auth: {type: password, value: secret}\n",
        );
        let (_, acc) = cfg.default_account().unwrap();
        assert!(!acc.show_unlisted_folders);
    }

    #[test]
    fn show_unlisted_folders_parses_when_explicitly_enabled() {
        let cfg = parse(
            "accounts:\n\
             \x20 work:\n\
             \x20   email: alice@corp.com\n\
             \x20   folders: [INBOX, Sent]\n\
             \x20   show_unlisted_folders: true\n\
             \x20   imap:\n\
             \x20     host: imap.corp.com\n\
             \x20     port: 993\n\
             \x20     login: alice@corp.com\n\
             \x20     auth: {type: password, value: secret}\n",
        );
        let (_, acc) = cfg.default_account().unwrap();
        assert!(acc.show_unlisted_folders);
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
    fn daemon_socket_path_defaults_to_none_when_unconfigured() {
        let cfg = parse(
            "accounts:\n\
             \x20 personal:\n\
             \x20   email: alice@example.com\n\
             \x20   imap:\n\
             \x20     host: imap.example.com\n\
             \x20     port: 993\n\
             \x20     login: alice@example.com\n\
             \x20     auth: {type: password, value: secret}\n",
        );
        assert!(cfg.daemon.is_none());
    }

    #[test]
    fn daemon_socket_path_parses_when_configured() {
        let cfg = parse(
            "daemon:\n\
             \x20 socket_path: /custom/run/jamaild.sock\n\
             accounts:\n\
             \x20 personal:\n\
             \x20   email: alice@example.com\n\
             \x20   imap:\n\
             \x20     host: imap.example.com\n\
             \x20     port: 993\n\
             \x20     login: alice@example.com\n\
             \x20     auth: {type: password, value: secret}\n",
        );
        assert_eq!(
            cfg.daemon.and_then(|d| d.socket_path),
            Some("/custom/run/jamaild.sock".to_string())
        );
    }

    #[test]
    fn caldav_server_listen_defaults_to_none() {
        let cfg = parse(
            "accounts:\n\
             \x20 personal:\n\
             \x20   email: alice@example.com\n\
             \x20   imap:\n\
             \x20     host: imap.example.com\n\
             \x20     port: 993\n\
             \x20     login: alice@example.com\n\
             \x20     auth: {type: password, value: secret}\n",
        );
        assert!(cfg.daemon.and_then(|d| d.caldav_server_listen).is_none());
    }

    #[test]
    fn caldav_server_listen_parses_when_configured() {
        let cfg = parse(
            "daemon:\n\
             \x20 caldav_server_listen: 127.0.0.1:5232\n\
             accounts:\n\
             \x20 personal:\n\
             \x20   email: alice@example.com\n\
             \x20   imap:\n\
             \x20     host: imap.example.com\n\
             \x20     port: 993\n\
             \x20     login: alice@example.com\n\
             \x20     auth: {type: password, value: secret}\n",
        );
        assert_eq!(
            cfg.daemon.and_then(|d| d.caldav_server_listen),
            Some("127.0.0.1:5232".to_string())
        );
    }

    #[test]
    fn week_start_defaults_to_monday_when_unconfigured() {
        let cfg = parse(
            "accounts:\n\
             \x20 personal:\n\
             \x20   email: alice@example.com\n\
             \x20   imap:\n\
             \x20     host: imap.example.com\n\
             \x20     port: 993\n\
             \x20     login: alice@example.com\n\
             \x20     auth: {type: password, value: secret}\n",
        );
        assert!(cfg.jacal.is_none());
        // The effective default (no jacal: block at all) must still be Monday.
        let week_start = cfg.jacal.map(|j| j.week_start).unwrap_or_default();
        assert_eq!(week_start, WeekStart::Monday);
    }

    #[test]
    fn week_start_parses_sunday_when_configured() {
        let cfg = parse(
            "jacal:\n\
             \x20 week_start: sunday\n\
             accounts:\n\
             \x20 personal:\n\
             \x20   email: alice@example.com\n\
             \x20   imap:\n\
             \x20     host: imap.example.com\n\
             \x20     port: 993\n\
             \x20     login: alice@example.com\n\
             \x20     auth: {type: password, value: secret}\n",
        );
        assert_eq!(cfg.jacal.unwrap().week_start, WeekStart::Sunday);
    }

    #[test]
    fn week_start_explicit_monday_parses_too() {
        let cfg = parse(
            "jacal:\n\
             \x20 week_start: monday\n\
             accounts:\n\
             \x20 personal:\n\
             \x20   email: alice@example.com\n\
             \x20   imap:\n\
             \x20     host: imap.example.com\n\
             \x20     port: 993\n\
             \x20     login: alice@example.com\n\
             \x20     auth: {type: password, value: secret}\n",
        );
        assert_eq!(cfg.jacal.unwrap().week_start, WeekStart::Monday);
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
        assert!(!work.show_unlisted_folders);
        assert_eq!(work.sent_folder.as_deref(), Some("Sent"));
        assert_eq!(work.draft_folder.as_deref(), Some("Drafts"));
        assert_eq!(
            work.notify_folders.as_deref(),
            Some(["INBOX".to_string()].as_slice())
        );
        let caldav = work.caldav.as_ref().expect("work.caldav documented");
        assert_eq!(caldav.url, "https://cal.example.com/dav/");
        assert_eq!(
            caldav.calendars,
            Some(vec!["Personal".to_string(), "Work".to_string()])
        );
        assert_eq!(caldav.poll_interval_secs, 300);
        assert_eq!(caldav.default_alarm_minutes_before, Some(vec![30, 5]));

        // The other example accounts must still parse with the new keys unset.
        let personal = &cfg.accounts["personal"];
        assert!(personal.senders.is_none());
        assert!(personal.sent_folder.is_none());
        assert!(personal.notify_folders.is_none());
        assert!(personal.caldav.is_none());
    }

    #[test]
    fn caldav_is_unset_by_default_and_backward_compatible() {
        let cfg = parse(
            "accounts:\n\
             \x20 personal:\n\
             \x20   email: alice@example.com\n\
             \x20   imap:\n\
             \x20     host: imap.example.com\n\
             \x20     port: 993\n\
             \x20     login: alice@example.com\n\
             \x20     auth: {type: password, value: secret}\n",
        );
        let (_, acc) = cfg.default_account().unwrap();
        assert!(acc.caldav.is_none());
    }

    #[test]
    fn caldav_parses_with_defaults_for_optional_fields() {
        let cfg = parse(
            "accounts:\n\
             \x20 personal:\n\
             \x20   email: alice@example.com\n\
             \x20   imap:\n\
             \x20     host: imap.example.com\n\
             \x20     port: 993\n\
             \x20     login: alice@example.com\n\
             \x20     auth: {type: password, value: secret}\n\
             \x20   caldav:\n\
             \x20     url: https://cal.example.com/dav/\n\
             \x20     login: alice@example.com\n\
             \x20     auth: {type: password, value: secret}\n",
        );
        let (_, acc) = cfg.default_account().unwrap();
        let caldav = acc.caldav.as_ref().expect("caldav present");
        assert_eq!(caldav.url, "https://cal.example.com/dav/");
        assert_eq!(caldav.poll_interval_secs, 300);
        assert!(caldav.calendars.is_none());
        assert!(caldav.default_alarm_minutes_before.is_none());
    }

    #[test]
    fn caldav_parses_explicit_optional_fields() {
        let cfg = parse(
            "accounts:\n\
             \x20 personal:\n\
             \x20   email: alice@example.com\n\
             \x20   imap:\n\
             \x20     host: imap.example.com\n\
             \x20     port: 993\n\
             \x20     login: alice@example.com\n\
             \x20     auth: {type: password, value: secret}\n\
             \x20   caldav:\n\
             \x20     url: https://cal.example.com/dav/\n\
             \x20     login: alice@example.com\n\
             \x20     auth: {type: command, value: 'pass show cal/alice'}\n\
             \x20     calendars: [Personal, Work]\n\
             \x20     poll_interval_secs: 60\n\
             \x20     default_alarm_minutes_before: [30, 5]\n",
        );
        let (_, acc) = cfg.default_account().unwrap();
        let caldav = acc.caldav.as_ref().expect("caldav present");
        assert_eq!(
            caldav.calendars,
            Some(vec!["Personal".to_string(), "Work".to_string()])
        );
        assert_eq!(caldav.poll_interval_secs, 60);
        assert_eq!(caldav.default_alarm_minutes_before, Some(vec![30, 5]));
    }
}
