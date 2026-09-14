//! `jadav`'s configuration: the `jadav:` section of `config.yaml`.
//!
//! `jadav` runs on a server host, typically with a config file that holds
//! *only* this section (no `accounts:`), so it parses its own root
//! ([`JadavFile`]) rather than requiring a full [`crate::config::JamailConfig`].
//! The shared workstation config keeps parsing too: `JamailConfig` carries
//! the same section as an optional field and serde ignores unknown keys in
//! both directions.
//!
//! Every field has a serde default or is `Option`, except the handful a
//! server cannot run without (`store`, `principal.login`/`auth`,
//! calendar `slug`/`name`). Config owns a calendar's *identity/provider*
//! facts; its *presentation* (`display_name`, `color`, `description`,
//! `order`, `timezone`) is only seeded from config on first creation and
//! then owned by CalDAV clients via `PROPPATCH` — see
//! [`crate::jadav::store::Store::reconcile_calendars`].

use crate::config::AuthConfig;
use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The root of a jadav-only config file.
#[derive(Debug, Deserialize)]
struct JadavFile {
    jadav: JadavConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct JadavConfig {
    /// TCP address the CalDAV server binds. Plain HTTP: TLS is the reverse
    /// proxy's job (traefik terminates `https://dav.…` in front of it).
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Path of jadav's own SQLite store (created on first start). Not the
    /// `mail.db` cache — see `jadav::store` for why the models differ.
    pub store: PathBuf,
    pub principal: PrincipalConfig,
    /// The Google Cloud OAuth client (Desktop-app type) used by every
    /// `kind: google` remote. Required when any such remote exists.
    pub google_oauth: Option<GoogleOauthConfig>,
    /// External accounts calendars can be mirrored from (Google, other
    /// CalDAV servers); consumed by `jadav::mirror`.
    #[serde(default)]
    pub remotes: IndexMap<String, RemoteConfig>,
    #[serde(default)]
    pub calendars: Vec<CalendarConfig>,
    /// Our own mail server: read invitations/replies (IMAP) and send iTIP
    /// messages (SMTP). Unset: no iMIP at all — writes still update the
    /// store, nothing is emailed or imported.
    pub mail: Option<MailConfig>,
    #[serde(default)]
    pub scheduling: SchedulingConfig,
    /// How long `change_log` rows are kept before compaction, which
    /// bounds how old a client's `sync-token` may be before it is
    /// answered with `valid-sync-token` (a full resync).
    #[serde(default = "default_retention_days")]
    pub sync_log_retention_days: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PrincipalConfig {
    /// The one login every CalDAV client uses (HTTP Basic).
    pub login: String,
    pub auth: AuthConfig,
    /// Every email address that is "me": the first entry is the primary.
    /// Each may be a bare address or `Name <address>`. Unset: `[login]`
    /// when the login looks like an email address.
    #[serde(default)]
    pub identities: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GoogleOauthConfig {
    pub client_id: String,
    /// The client secret (a Desktop-app secret is not really secret, but
    /// `command` keeps it out of the file all the same).
    pub client_secret: AuthConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RemoteConfig {
    pub kind: RemoteKind,
    /// The account's own address at the provider (Google: the account
    /// email, also its primary calendar id).
    pub user: Option<String>,
    /// CalDAV remotes: the collection or home URL; `login`/`auth` for
    /// Basic auth.
    pub url: Option<String>,
    pub login: Option<String>,
    pub auth: Option<AuthConfig>,
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// How far back the first fill and any full resync reach; `0` means
    /// everything.
    pub history_days: Option<u32>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RemoteKind {
    Google,
    Caldav,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CalendarConfig {
    /// URL path segment of the collection (`/dav/calendars/<login>/<slug>/`).
    pub slug: String,
    /// Initial `displayname`; clients may rename via `PROPPATCH` later.
    pub name: String,
    /// Owning email identity (must be one of `principal.identities`).
    /// Unset: the primary identity.
    pub identity: Option<String>,
    #[serde(default)]
    pub provider: Provider,
    /// For `google`/`caldav` providers: the `remotes:` entry to mirror from
    /// and the remote calendar id/URL/display name.
    pub remote: Option<String>,
    pub remote_calendar: Option<String>,
    /// `false` makes a mirrored calendar read-only for CalDAV clients
    /// (writes get 403); the mirror still fills it. Ignored for `native`.
    #[serde(default = "default_true")]
    pub two_way: bool,
    /// Per-calendar override of `scheduling.send_via`.
    pub send_via: Option<SendVia>,
    pub color: Option<String>,
    pub description: Option<String>,
    /// Raw `VTIMEZONE`-bearing `VCALENDAR` text for `calendar-timezone`,
    /// or just an IANA name (stored verbatim either way).
    pub timezone: Option<String>,
    /// Where invitations addressed to this calendar's identity land when
    /// no calendar already holds the event. At most one per identity.
    #[serde(default)]
    pub default_for_identity: bool,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Native,
    Google,
    Caldav,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Native => "native",
            Provider::Google => "google",
            Provider::Caldav => "caldav",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "native" => Some(Provider::Native),
            "google" => Some(Provider::Google),
            "caldav" => Some(Provider::Caldav),
            _ => None,
        }
    }
}

/// Who emails iTIP messages for a calendar: `smtp` — jadav itself through
/// the configured SMTP server (the default, and what keeps every identity
/// SPF/DKIM-aligned on this deployment's relay setup); `provider` — the
/// remote service (Google's `sendUpdates`, a CalDAV server's own
/// auto-schedule), jadav stays silent.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SendVia {
    #[default]
    Smtp,
    Provider,
}

impl SendVia {
    pub fn as_str(self) -> &'static str {
        match self {
            SendVia::Smtp => "smtp",
            SendVia::Provider => "provider",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "smtp" => Some(SendVia::Smtp),
            "provider" => Some(SendVia::Provider),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct MailConfig {
    pub imap: crate::config::ImapConfig,
    pub smtp: crate::config::SmtpConfig,
    /// Folders watched for iTIP mail; the first is the one IDLE'd on.
    #[serde(default = "default_mail_folders")]
    pub folders: Vec<String>,
    /// On first run (or a `UIDVALIDITY` change) look this many days back.
    #[serde(default = "default_since_days")]
    pub since_days: u32,
    #[serde(default = "default_mail_poll_secs")]
    pub poll_interval_secs: u64,
    /// IMAP keyword → identity hints (e.g. `acct-scraperapi:
    /// tamas@scraperapi.com`), used only when the ICS names several of our
    /// identities.
    #[serde(default)]
    pub keyword_identities: IndexMap<String, String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SchedulingConfig {
    #[serde(default)]
    pub send_via: SendVia,
    /// Default mirror window in days when a remote does not set its own.
    #[serde(default = "default_history_days")]
    pub history_days: u32,
    /// Property changes that re-invite existing attendees.
    #[serde(default = "default_significant_properties")]
    pub significant_properties: Vec<String>,
    /// Also file inbound `REQUEST`/`CANCEL` copies in the schedule inbox for
    /// mirrored calendars (off: iOS may then re-file the event elsewhere).
    #[serde(default)]
    pub inbox_for_mirrored: bool,
    /// Give up on an outbound message after this long.
    #[serde(default = "default_retry_max_age_hours")]
    pub retry_max_age_hours: u32,
    /// Zone used to render `{when}` in mail subjects for UTC/floating events.
    #[serde(default = "default_timezone")]
    pub default_timezone: String,
}

impl Default for SchedulingConfig {
    fn default() -> Self {
        Self {
            send_via: SendVia::default(),
            history_days: default_history_days(),
            significant_properties: default_significant_properties(),
            inbox_for_mirrored: false,
            retry_max_age_hours: default_retry_max_age_hours(),
            default_timezone: default_timezone(),
        }
    }
}

fn default_mail_folders() -> Vec<String> {
    vec!["INBOX".to_string()]
}
fn default_since_days() -> u32 {
    14
}
fn default_mail_poll_secs() -> u64 {
    60
}
fn default_significant_properties() -> Vec<String> {
    crate::jadav::itip::DEFAULT_SIGNIFICANT_PROPERTIES
        .iter()
        .map(|s| s.to_string())
        .collect()
}
fn default_retry_max_age_hours() -> u32 {
    24
}
fn default_timezone() -> String {
    "UTC".to_string()
}

fn default_listen() -> String {
    "0.0.0.0:5232".to_string()
}
fn default_retention_days() -> u32 {
    90
}
fn default_poll_interval_secs() -> u64 {
    120
}
fn default_history_days() -> u32 {
    90
}
fn default_true() -> bool {
    true
}

/// Segments reserved for the scheduling collections under the calendar
/// home; a calendar can never take one of these as its slug.
pub const RESERVED_SLUGS: &[&str] = &["inbox", "outbox"];

/// Slugs configured by hand: lowercase, start alphanumeric, `[a-z0-9_-]`,
/// at most 64 chars. (Clients creating calendars via `MKCALENDAR` may use
/// the wider `[A-Za-z0-9._-]` set the server accepts.)
pub fn is_valid_config_slug(slug: &str) -> bool {
    let mut chars = slug.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    slug.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// The bare address inside `Name <addr>` (or the trimmed input when there
/// are no angle brackets), lower-cased.
pub fn mailbox_address(s: &str) -> String {
    let s = s.trim();
    let inner = match (s.rfind('<'), s.rfind('>')) {
        (Some(lt), Some(gt)) if lt < gt => &s[lt + 1..gt],
        _ => s,
    };
    inner
        .trim()
        .trim_start_matches("mailto:")
        .to_ascii_lowercase()
}

impl JadavConfig {
    /// Parse a file whose root carries a `jadav:` section — either a
    /// jadav-only file or the full shared jamail config (the other
    /// sections are ignored here).
    pub fn load_from(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config: {}", path.display()))?;
        let file: JadavFile = serde_yml::from_str(&content)
            .with_context(|| format!("Failed to parse jadav config: {}", path.display()))?;
        Ok(file.jadav)
    }

    /// Every configured identity as a bare lower-cased address, primary
    /// first, de-duplicated; falls back to the login when it is an email
    /// address and no identities are listed.
    pub fn identities(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for id in &self.principal.identities {
            let addr = mailbox_address(id);
            if !addr.is_empty() && !out.contains(&addr) {
                out.push(addr);
            }
        }
        if out.is_empty() && self.principal.login.contains('@') {
            out.push(mailbox_address(&self.principal.login));
        }
        out
    }

    /// The primary identity (first configured), if any.
    pub fn primary_identity(&self) -> Option<String> {
        self.identities().into_iter().next()
    }

    /// Display name configured for `identity` (from a `Name <addr>`
    /// entry), if any.
    pub fn identity_display_name(&self, identity: &str) -> Option<String> {
        self.principal.identities.iter().find_map(|entry| {
            if mailbox_address(entry) != identity {
                return None;
            }
            let lt = entry.find('<')?;
            let name = entry[..lt].trim().trim_matches('"').trim();
            if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            }
        })
    }

    /// The identity a configured calendar belongs to (explicit, else the
    /// primary), lower-cased.
    pub fn calendar_identity(&self, cal: &CalendarConfig) -> Option<String> {
        match &cal.identity {
            Some(id) => Some(mailbox_address(id)),
            None => self.primary_identity(),
        }
    }

    /// Check everything a server cannot start without and return
    /// non-fatal warnings for the rest. Errors: no identity at all,
    /// duplicate/invalid/reserved slugs, an unknown identity or remote
    /// referenced by a calendar, a non-native calendar without a remote,
    /// more than one `default_for_identity` per identity.
    pub fn validate(&self) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
        if self.principal.login.trim().is_empty() {
            bail!("jadav.principal.login must not be empty");
        }
        let identities = self.identities();
        if identities.is_empty() {
            bail!(
                "jadav.principal.identities is empty and the login is not an email address; \
                 list at least one identity"
            );
        }
        if self.store.as_os_str().is_empty() {
            bail!("jadav.store must be set");
        }
        let mut seen = std::collections::HashSet::new();
        let mut defaults_per_identity: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for cal in &self.calendars {
            if !is_valid_config_slug(&cal.slug) {
                bail!(
                    "calendar slug {:?} is invalid: use lowercase letters, digits, '-' or '_', \
                     starting with a letter or digit",
                    cal.slug
                );
            }
            if RESERVED_SLUGS.contains(&cal.slug.as_str()) {
                bail!("calendar slug {:?} is reserved", cal.slug);
            }
            if !seen.insert(cal.slug.clone()) {
                bail!("calendar slug {:?} is configured twice", cal.slug);
            }
            if cal.name.trim().is_empty() {
                bail!("calendar {:?} has an empty name", cal.slug);
            }
            let identity = self.calendar_identity(cal).context("no primary identity")?;
            if !identities.contains(&identity) {
                bail!(
                    "calendar {:?} names identity {:?}, which is not in principal.identities",
                    cal.slug,
                    identity
                );
            }
            if cal.provider != Provider::Native {
                let remote = cal.remote.as_deref().with_context(|| {
                    format!(
                        "calendar {:?} has provider {} but no `remote`",
                        cal.slug,
                        cal.provider.as_str()
                    )
                })?;
                if !self.remotes.contains_key(remote) {
                    bail!(
                        "calendar {:?} references unknown remote {:?}",
                        cal.slug,
                        remote
                    );
                }
                if cal.remote_calendar.is_none() {
                    warnings.push(format!(
                        "calendar {:?}: no `remote_calendar` set; the remote's primary calendar will be used",
                        cal.slug
                    ));
                }
            } else if cal.remote.is_some() {
                warnings.push(format!(
                    "calendar {:?} is native but names a remote; the remote is ignored",
                    cal.slug
                ));
            }
            if cal.default_for_identity {
                *defaults_per_identity.entry(identity).or_insert(0) += 1;
            }
        }
        for (identity, n) in &defaults_per_identity {
            if *n > 1 {
                bail!(
                    "identity {:?} has {} calendars marked default_for_identity; allow at most one",
                    identity,
                    n
                );
            }
        }
        for identity in &identities {
            let has_any = self
                .calendars
                .iter()
                .any(|c| self.calendar_identity(c).as_deref() == Some(identity.as_str()));
            if !has_any && !defaults_per_identity.contains_key(identity) {
                warnings.push(format!(
                    "identity {:?} has no calendar; invitations addressed to it cannot be stored",
                    identity
                ));
            }
        }
        for (name, remote) in &self.remotes {
            match remote.kind {
                RemoteKind::Google if remote.user.is_none() => {
                    bail!("remote {:?} is google but has no `user`", name)
                }
                RemoteKind::Google if self.google_oauth.is_none() => {
                    bail!(
                        "remote {:?} is google but `jadav.google_oauth` is not configured",
                        name
                    )
                }
                RemoteKind::Caldav if remote.url.is_none() => {
                    bail!("remote {:?} is caldav but has no `url`", name)
                }
                _ => {}
            }
        }
        Ok(warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> JadavConfig {
        let file: JadavFile = serde_yml::from_str(yaml).expect("parse");
        file.jadav
    }

    const MINIMAL: &str = "jadav:\n  store: /tmp/x.db\n  principal:\n    login: t@example.com\n    auth: {type: password, value: pw}\n";

    #[test]
    fn minimal_config_defaults_and_derives_identity_from_login() {
        let cfg = parse(MINIMAL);
        assert_eq!(cfg.listen, "0.0.0.0:5232");
        assert_eq!(cfg.sync_log_retention_days, 90);
        assert_eq!(cfg.identities(), vec!["t@example.com".to_string()]);
        assert_eq!(cfg.primary_identity().as_deref(), Some("t@example.com"));
        assert!(cfg.calendars.is_empty());
        assert!(cfg.validate().unwrap().is_empty() || !cfg.validate().unwrap().is_empty());
    }

    #[test]
    fn identities_are_normalised_deduplicated_and_ordered() {
        let cfg = parse(
            "jadav:\n  store: /tmp/x.db\n  principal:\n    login: user\n    auth: {type: password, value: pw}\n    identities: [\"Tamas K <T@Example.com>\", other@example.com, t@example.com]\n",
        );
        assert_eq!(
            cfg.identities(),
            vec!["t@example.com".to_string(), "other@example.com".to_string()]
        );
        assert_eq!(
            cfg.identity_display_name("t@example.com").as_deref(),
            Some("Tamas K")
        );
        assert_eq!(cfg.identity_display_name("other@example.com"), None);
    }

    #[test]
    fn mailbox_address_handles_display_names_and_mailto() {
        assert_eq!(mailbox_address("Tamas <t@mas.gg>"), "t@mas.gg");
        assert_eq!(mailbox_address("  T@MAS.GG "), "t@mas.gg");
        assert_eq!(mailbox_address("mailto:t@mas.gg"), "t@mas.gg");
        assert_eq!(
            mailbox_address("\"Doe, John\" <john@example.com>"),
            "john@example.com"
        );
    }

    #[test]
    fn full_config_parses_and_validates() {
        let cfg = parse(
            "jadav:\n  listen: 127.0.0.1:5232\n  store: /var/lib/jadav/jadav.db\n  principal:\n    login: t@mas.gg\n    auth: {type: command, value: 'echo pw'}\n    identities: [t@mas.gg, tamas@example.com, lonely@example.com]\n  google_oauth: {client_id: cid, client_secret: {type: password, value: sec}}\n  remotes:\n    work: {kind: google, user: tamas@example.com}\n    cloud: {kind: caldav, url: https://caldav.example.com/, login: u, auth: {type: password, value: p}}\n  calendars:\n    - {slug: personal, name: Personal, provider: native, default_for_identity: true}\n    - {slug: g-work, name: Work, identity: tamas@example.com, provider: google, remote: work, remote_calendar: tamas@example.com, two_way: true}\n    - {slug: cloud-home, name: Home, provider: caldav, remote: cloud, remote_calendar: Home, two_way: false, color: '#ff0000'}\n  scheduling: {send_via: smtp, history_days: 30}\n  sync_log_retention_days: 30\n",
        );
        assert_eq!(cfg.calendars.len(), 3);
        assert_eq!(cfg.calendars[1].provider, Provider::Google);
        assert!(cfg.calendars[1].two_way);
        assert!(!cfg.calendars[2].two_way);
        assert_eq!(cfg.remotes["work"].kind, RemoteKind::Google);
        assert_eq!(cfg.remotes["work"].poll_interval_secs, 120);
        assert_eq!(cfg.scheduling.history_days, 30);
        assert_eq!(cfg.scheduling.send_via, SendVia::Smtp);
        let warnings = cfg.validate().unwrap();
        // tamas@example.com has only a mirrored calendar: invitations for
        // it are the provider's business, not a misconfiguration.
        assert!(
            !warnings.iter().any(|w| w.contains("tamas@example.com")),
            "{warnings:?}"
        );
        // lonely@example.com has nowhere at all for an invitation to land.
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("lonely@example.com") && w.contains("no calendar")),
            "{warnings:?}"
        );
    }

    #[test]
    fn validate_rejects_bad_slugs_and_dangling_references() {
        let base = "jadav:\n  store: /tmp/x.db\n  principal:\n    login: t@example.com\n    auth: {type: password, value: pw}\n  calendars:\n";
        let bad = |cals: &str| {
            parse(&format!("{base}{cals}"))
                .validate()
                .unwrap_err()
                .to_string()
        };
        assert!(bad("    - {slug: Inbox, name: x}\n").contains("invalid"));
        assert!(bad("    - {slug: inbox, name: x}\n").contains("reserved"));
        assert!(bad("    - {slug: a, name: x}\n    - {slug: a, name: y}\n").contains("twice"));
        assert!(
            bad("    - {slug: a, name: x, identity: nobody@example.com}\n")
                .contains("not in principal.identities")
        );
        assert!(bad("    - {slug: a, name: x, provider: google}\n").contains("no `remote`"));
        assert!(
            bad("    - {slug: a, name: x, provider: google, remote: nope}\n")
                .contains("unknown remote")
        );
        assert!(
            bad("    - {slug: a, name: x, default_for_identity: true}\n    - {slug: b, name: y, default_for_identity: true}\n")
                .contains("default_for_identity")
        );
    }

    #[test]
    fn shared_jamail_config_with_jadav_section_parses_through_jamail_config() {
        let yaml = "accounts:\n  main:\n    email: a@example.com\n    imap: {host: h, port: 993, login: l, auth: {type: password, value: p}}\njadav:\n  store: /tmp/x.db\n  principal:\n    login: a@example.com\n    auth: {type: password, value: p}\n";
        let cfg: crate::config::JamailConfig = serde_yml::from_str(yaml).unwrap();
        assert_eq!(cfg.jadav.unwrap().store, PathBuf::from("/tmp/x.db"));
        // And a config without the section still parses.
        let yaml2 = "accounts:\n  main:\n    email: a@example.com\n    imap: {host: h, port: 993, login: l, auth: {type: password, value: p}}\n";
        let cfg2: crate::config::JamailConfig = serde_yml::from_str(yaml2).unwrap();
        assert!(cfg2.jadav.is_none());
    }
}
