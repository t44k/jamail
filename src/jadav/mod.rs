//! `jadav` — a standalone, self-hosted CalDAV server daemon.
//!
//! Where `jamaild` is a *client* of CalDAV servers (it caches remote
//! calendars for the TUIs), `jadav` *is* the server: it owns the calendars
//! the phone, `jamaild` and any other CalDAV client connect to, mirrors
//! selected calendars two-way with external providers (Google, other
//! CalDAV servers — see `mirror`), and in later milestones speaks iTIP over
//! iMIP with the local mail server for invitations. It shares this crate's
//! iCalendar (`calendar`), HTTP (`httpc`) and CalDAV-client (`caldav`)
//! code but has its own store ([`store`]) and no IPC socket.
//!
//! ```text
//! jadav [--config <path>] [serve]
//! jadav [--config <path>] check-config
//! jadav [--config <path>] health
//! jadav [--config <path>] import-caldav <base-url> --calendar <slug> [--remote-calendar <name>]
//!                                       --login <login> (--password <pw> | --password-command <cmd>)
//! ```
//!
//! Config path: `--config`, else `$JAMAIL_CONFIG`, else
//! `~/.config/jamail/config.yaml`; only the `jadav:` section is read (see
//! [`config`]). `serve` blocks until `SIGTERM`/`SIGINT`; `SIGHUP`
//! re-resolves the principal's password (so a rotated `auth: command`
//! secret is picked up without a restart).

pub mod caldav_remote;
pub mod config;
pub mod google;
pub mod imip;
pub mod itip;
pub mod mirror;
pub mod server;
pub mod store;
pub mod xml;

use crate::caldav::CalDavClient;
use crate::config::AuthConfig;
use anyhow::{Context, Result, bail};
use config::{CalendarConfig, JadavConfig, Provider, RemoteConfig, RemoteKind};
use google::GCalendarListEntry;
use mirror::{CalendarMirrorCfg, Discoverer, MirrorControl, MirrorHub, ProviderFactory};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use store::{CalendarSpec, Origin, Precondition, Store};

const COMPACTION_INTERVAL: Duration = Duration::from_secs(60 * 60);

static TERM_RECEIVED: AtomicBool = AtomicBool::new(false);
static HUP_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_sig: libc::c_int) {
    TERM_RECEIVED.store(true, Ordering::SeqCst);
}

extern "C" fn on_hup(_sig: libc::c_int) {
    HUP_RECEIVED.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as usize);
        libc::signal(libc::SIGINT, on_term as *const () as usize);
        libc::signal(libc::SIGHUP, on_hup as *const () as usize);
    }
}

/// Parsed command line.
#[derive(Debug, PartialEq, Eq)]
pub struct Cli {
    pub config: Option<PathBuf>,
    pub command: Command,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Serve,
    CheckConfig,
    Health,
    ImportCaldav {
        base_url: String,
        slug: String,
        remote_calendar: Option<String>,
        login: String,
        password: Option<String>,
        password_command: Option<String>,
    },
    /// `google auth <remote>` | `google import-token <remote> <state.json>` |
    /// `google calendars <remote>`.
    Google(Vec<String>),
    /// Mirror health per remote.
    Status,
    /// One mirror pass for one remote, in the foreground.
    MirrorRunOnce {
        remote: String,
    },
    /// Re-list and re-compare every object of a remote's calendars (after
    /// a mirror-mapping change) — see `Store::force_mirror_refresh`.
    MirrorRefresh {
        remote: String,
    },
}

pub fn parse_cli(args: &[String]) -> Result<Cli> {
    let mut config = None;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                i += 1;
                let p = args.get(i).context("--config needs a path")?;
                config = Some(PathBuf::from(p));
            }
            "-h" | "--help" => {
                bail!("{}", USAGE.trim());
            }
            other => rest.push(other.to_string()),
        }
        i += 1;
    }
    let command = match rest.first().map(String::as_str) {
        None | Some("serve") => Command::Serve,
        Some("check-config") => Command::CheckConfig,
        Some("health") => Command::Health,
        Some("google") => Command::Google(rest[1..].to_vec()),
        Some("status") => Command::Status,
        Some("mirror") => match rest.get(1).map(String::as_str) {
            Some("run-once") => Command::MirrorRunOnce {
                remote: rest
                    .get(2)
                    .cloned()
                    .context("mirror run-once needs a <remote>")?,
            },
            Some("refresh") => Command::MirrorRefresh {
                remote: rest
                    .get(2)
                    .cloned()
                    .context("mirror refresh needs a <remote>")?,
            },
            _ => bail!("usage: jadav mirror run-once|refresh <remote>"),
        },
        Some("import-caldav") => {
            let mut base_url = None;
            let mut slug = None;
            let mut remote_calendar = None;
            let mut login = None;
            let mut password = None;
            let mut password_command = None;
            let mut j = 1;
            while j < rest.len() {
                let take = |j: &mut usize| -> Result<String> {
                    *j += 1;
                    rest.get(*j)
                        .cloned()
                        .with_context(|| format!("{} needs a value", rest[*j - 1]))
                };
                match rest[j].as_str() {
                    "--calendar" => slug = Some(take(&mut j)?),
                    "--remote-calendar" => remote_calendar = Some(take(&mut j)?),
                    "--login" => login = Some(take(&mut j)?),
                    "--password" => password = Some(take(&mut j)?),
                    "--password-command" => password_command = Some(take(&mut j)?),
                    v if base_url.is_none() && !v.starts_with("--") => {
                        base_url = Some(v.to_string())
                    }
                    other => bail!("unexpected argument {:?}\n{}", other, USAGE.trim()),
                }
                j += 1;
            }
            Command::ImportCaldav {
                base_url: base_url.context("import-caldav needs a <base-url>")?,
                slug: slug.context("import-caldav needs --calendar <slug>")?,
                remote_calendar,
                login: login.context("import-caldav needs --login")?,
                password,
                password_command,
            }
        }
        Some(other) => bail!("unknown command {:?}\n{}", other, USAGE.trim()),
    };
    Ok(Cli { config, command })
}

const USAGE: &str = "
usage: jadav [--config <path>] [serve]
       jadav [--config <path>] check-config
       jadav [--config <path>] health
       jadav [--config <path>] status
       jadav [--config <path>] import-caldav <base-url> --calendar <slug> [--remote-calendar <name>]
                               --login <login> (--password <pw> | --password-command <cmd>)
       jadav [--config <path>] google auth <remote>
       jadav [--config <path>] google import-token <remote> <state.json>
       jadav [--config <path>] google calendars <remote>
       jadav [--config <path>] mirror run-once <remote>
";

pub fn run(args: Vec<String>) -> Result<()> {
    let cli = parse_cli(&args)?;
    let config_path = match cli.config {
        Some(p) => p,
        None => crate::config::config_path()?,
    };
    match cli.command {
        Command::Serve => serve(&config_path),
        Command::CheckConfig => check_config(&config_path),
        Command::Health => health(&config_path),
        Command::ImportCaldav {
            base_url,
            slug,
            remote_calendar,
            login,
            password,
            password_command,
        } => {
            let auth = match (password, password_command) {
                (Some(p), _) => AuthConfig {
                    auth_type: "password".to_string(),
                    value: p,
                },
                (None, Some(c)) => AuthConfig {
                    auth_type: "command".to_string(),
                    value: c,
                },
                (None, None) => bail!("import-caldav needs --password or --password-command"),
            };
            import_caldav(
                &config_path,
                &base_url,
                &slug,
                remote_calendar.as_deref(),
                &login,
                &auth,
            )
        }
        Command::Google(args) => google_command(&config_path, &args),
        Command::Status => status(&config_path),
        Command::MirrorRunOnce { remote } => mirror_run_once(&config_path, &remote),
        Command::MirrorRefresh { remote } => mirror_refresh(&config_path, &remote),
    }
}

fn google_oauth_secret(cfg: &JadavConfig) -> Result<(String, String)> {
    let oauth = cfg
        .google_oauth
        .as_ref()
        .context("jadav.google_oauth is not configured")?;
    let secret = oauth
        .client_secret
        .resolve_password()
        .context("resolving jadav.google_oauth.client_secret")?;
    Ok((oauth.client_id.clone(), secret))
}

fn google_remote<'a>(cfg: &'a JadavConfig, name: &str) -> Result<&'a RemoteConfig> {
    let remote = cfg
        .remotes
        .get(name)
        .with_context(|| format!("no remote named {:?} in jadav.remotes", name))?;
    if remote.kind != RemoteKind::Google {
        bail!("remote {:?} is not a google remote", name);
    }
    Ok(remote)
}

fn google_command(config_path: &Path, args: &[String]) -> Result<()> {
    let cfg = load(config_path)?;
    let store = open_store(&cfg)?;
    let (client_id, secret) = google_oauth_secret(&cfg)?;
    match args.first().map(String::as_str) {
        Some("auth") => {
            let name = args.get(1).context("google auth needs a <remote>")?;
            let remote = google_remote(&cfg, name)?;
            let hint = remote.user.clone().unwrap_or_default();
            google::auth_interactive(&store, name, &client_id, &secret, &hint)
        }
        Some("import-token") => {
            let name = args
                .get(1)
                .context("google import-token needs a <remote>")?;
            let path = args
                .get(2)
                .context("google import-token needs a <state.json>")?;
            google_remote(&cfg, name)?;
            google::import_token(
                &store,
                &cfg.store,
                name,
                &client_id,
                &secret,
                Path::new(path),
            )
        }
        Some("calendars") => {
            let name = args.get(1).context("google calendars needs a <remote>")?;
            google_remote(&cfg, name)?;
            let auth =
                google::GoogleAuth::from_store(&store, &cfg.store, name, &client_id, &secret)?;
            let mut client = google::GoogleClient::new(Arc::new(Mutex::new(auth)));
            for cal in google::list_calendars(&mut client)? {
                println!(
                    "{}{}  {}  ({})",
                    cal.id,
                    if cal.primary == Some(true) {
                        " [primary]"
                    } else {
                        ""
                    },
                    cal.summary.unwrap_or_default(),
                    cal.access_role.unwrap_or_default()
                );
            }
            Ok(())
        }
        _ => bail!("usage: jadav google auth|import-token|calendars <remote> …"),
    }
}

fn status(config_path: &Path) -> Result<()> {
    let cfg = load(config_path)?;
    let store = Store::open(&cfg.store)?;
    let lines = mirror::status_lines(&store)?;
    if lines.is_empty() {
        println!("no mirror activity recorded yet");
    }
    for l in lines {
        println!("{}", l);
    }
    let (pending, sent, failed) = store.outbound_counts()?;
    println!(
        "outbound mail: {} pending, {} sent, {} failed; inbox items: {}",
        pending,
        sent,
        failed,
        store.inbox_list()?.len()
    );
    for cal in store.list_calendars()? {
        if cal.provider != Provider::Native {
            let st = store.calendar_mirror_state(&cal.slug)?;
            println!(
                "calendar {}: {} objects, token {}, last full fill {}",
                cal.slug,
                store.list_objects(&cal.slug)?.len(),
                st.remote_sync_token
                    .as_deref()
                    .map(|_| "yes")
                    .unwrap_or("none"),
                st.last_full_fill_at
                    .and_then(|t| chrono::DateTime::<chrono::Utc>::from_timestamp(t, 0))
                    .map(|t| t.format("%Y-%m-%d %H:%M:%SZ").to_string())
                    .unwrap_or_else(|| "never".to_string())
            );
        }
    }
    Ok(())
}

/// Mirror configs for every calendar that belongs to `remote_name`.
fn mirror_cfgs(cfg: &JadavConfig, remote_name: &str) -> Result<Vec<CalendarMirrorCfg>> {
    let remote = cfg
        .remotes
        .get(remote_name)
        .with_context(|| format!("no remote {:?}", remote_name))?;
    let history_days = remote.history_days.unwrap_or(cfg.scheduling.history_days);
    Ok(calendar_specs(cfg)?
        .into_iter()
        .filter(|s| {
            s.remote_account.as_deref() == Some(remote_name) && s.provider != Provider::Native
        })
        .map(|s| CalendarMirrorCfg {
            remote_calendar: s
                .remote_calendar_id
                .clone()
                .or_else(|| remote.user.clone())
                .unwrap_or_default(),
            slug: s.slug,
            identity: s.identity,
            two_way: s.two_way,
            send_via: s.send_via,
            history_days,
        })
        .collect())
}

/// Mirror configs for the calendars `mirror_all` discovered on
/// `remote_name` and recorded in the store, so a restart mirrors them
/// straight away instead of waiting for the next calendar-list lookup.
fn discovered_mirror_cfgs(
    cfg: &JadavConfig,
    store: &Store,
    remote_name: &str,
) -> Result<Vec<CalendarMirrorCfg>> {
    let remote = cfg
        .remotes
        .get(remote_name)
        .with_context(|| format!("no remote {:?}", remote_name))?;
    let history_days = remote.history_days.unwrap_or(cfg.scheduling.history_days);
    Ok(store
        .list_discovered_calendars(remote_name)?
        .into_iter()
        .map(|row| CalendarMirrorCfg {
            remote_calendar: row.remote_calendar_id.clone().unwrap_or_default(),
            slug: row.slug,
            identity: row.identity,
            two_way: row.two_way,
            send_via: row.send_via,
            history_days,
        })
        .collect())
}

/// Configured plus discovered mirror configs for one remote.
fn all_mirror_cfgs(
    cfg: &JadavConfig,
    store: &Store,
    remote_name: &str,
) -> Result<Vec<CalendarMirrorCfg>> {
    let mut cals = mirror_cfgs(cfg, remote_name)?;
    for extra in discovered_mirror_cfgs(cfg, store, remote_name)? {
        if !cals.iter().any(|c| c.slug == extra.slug) {
            cals.push(extra);
        }
    }
    Ok(cals)
}

/// A URL-safe slug fragment from a calendar name: lower-case ASCII
/// letters and digits, runs of anything else collapsed to one `-`, at most
/// 40 characters, never empty.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in name.chars() {
        let folded = fold_latin(ch);
        if folded.is_ascii_alphanumeric() {
            out.push(folded.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() >= 40 {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "calendar".to_string()
    } else {
        out
    }
}

/// Strip the accent off a Latin letter (`ü` → `u`, `ő` → `o`, `ß` → `s`)
/// so a Hungarian or German calendar name still yields a readable slug;
/// anything else is returned unchanged and dropped by [`slugify`].
fn fold_latin(ch: char) -> char {
    match ch {
        'à'..='å' | 'À'..='Å' | 'ā' | 'Ā' | 'ă' | 'Ă' | 'ą' | 'Ą' => 'a',
        'æ' | 'Æ' => 'a',
        'ç' | 'Ç' | 'ć' | 'Ć' | 'č' | 'Č' => 'c',
        'ď' | 'Ď' | 'đ' | 'Đ' | 'ð' | 'Ð' => 'd',
        'è'..='ë' | 'È'..='Ë' | 'ē' | 'Ē' | 'ė' | 'Ė' | 'ę' | 'Ę' | 'ě' | 'Ě' => 'e',
        'ì'..='ï' | 'Ì'..='Ï' | 'ī' | 'Ī' | 'ı' | 'İ' => 'i',
        'ľ' | 'Ľ' | 'ł' | 'Ł' | 'ĺ' | 'Ĺ' => 'l',
        'ñ' | 'Ñ' | 'ń' | 'Ń' | 'ň' | 'Ň' => 'n',
        'ò'..='ö' | 'Ò'..='Ö' | 'ø' | 'Ø' | 'ō' | 'Ō' | 'ő' | 'Ő' => 'o',
        'ř' | 'Ř' | 'ŕ' | 'Ŕ' => 'r',
        'ś' | 'Ś' | 'š' | 'Š' | 'ş' | 'Ş' | 'ß' => 's',
        'ť' | 'Ť' | 'ţ' | 'Ţ' | 'þ' | 'Þ' => 't',
        'ù'..='ü' | 'Ù'..='Ü' | 'ū' | 'Ū' | 'ů' | 'Ů' | 'ű' | 'Ű' => 'u',
        'ý' | 'Ý' | 'ÿ' | 'Ÿ' => 'y',
        'ž' | 'Ž' | 'ź' | 'Ź' | 'ż' | 'Ż' => 'z',
        other => other,
    }
}

/// Whether a configured calendar already covers Google calendar `id` of
/// `remote_name` (so `mirror_all` must leave it alone).
fn configured_covers(cfg: &JadavConfig, remote_name: &str, entry: &GCalendarListEntry) -> bool {
    let remote_user = cfg
        .remotes
        .get(remote_name)
        .and_then(|r| r.user.as_deref())
        .map(str::to_ascii_lowercase);
    cfg.calendars.iter().any(|c| {
        if c.remote.as_deref() != Some(remote_name) {
            return false;
        }
        // An explicit calendar without `remote_calendar` means the primary.
        let configured_id = c
            .remote_calendar
            .as_deref()
            .map(str::to_ascii_lowercase)
            .or_else(|| remote_user.clone());
        let is_primary =
            entry.primary == Some(true) && configured_id.as_deref() == remote_user.as_deref();
        configured_id.as_deref() == Some(entry.id.to_ascii_lowercase().as_str()) || is_primary
    })
}

/// The store spec `mirror_all` creates for one Google calendar-list
/// entry, or `None` when the entry is not mirrored: deleted or hidden on
/// Google, free/busy-only, excluded by `skip_calendars`, or already
/// configured explicitly. `taken` holds the slugs already in use; a
/// clash gets a short hash of the remote id appended.
pub fn discovered_spec(
    cfg: &JadavConfig,
    remote_name: &str,
    entry: &GCalendarListEntry,
    taken: &HashSet<String>,
) -> Option<CalendarSpec> {
    let remote = cfg.remotes.get(remote_name)?;
    if entry.deleted == Some(true) || entry.hidden == Some(true) {
        return None;
    }
    if !matches!(
        entry.access_role.as_deref(),
        Some("owner") | Some("writer") | Some("reader")
    ) {
        return None;
    }
    let name = entry.name();
    if remote
        .skip_calendars
        .iter()
        .any(|s| s.eq_ignore_ascii_case(&entry.id) || s.eq_ignore_ascii_case(name))
    {
        return None;
    }
    if configured_covers(cfg, remote_name, entry) {
        return None;
    }
    let prefix = remote
        .slug_prefix
        .clone()
        .unwrap_or_else(|| format!("g-{}", slugify(remote_name)));
    let base = format!("{}-{}", prefix, slugify(name));
    let slug = if taken.contains(&base) {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(entry.id.as_bytes());
        format!(
            "{}-{:02x}{:02x}{:02x}",
            base, digest[0], digest[1], digest[2]
        )
    } else {
        base
    };
    let identity = remote
        .user
        .as_deref()
        .map(|u| u.to_ascii_lowercase())
        .or_else(|| cfg.primary_identity())?;
    Some(CalendarSpec {
        slug,
        display_name: format!("{} ({})", name, remote_name),
        description: entry.description.clone().filter(|d| !d.trim().is_empty()),
        color: entry.background_color.clone(),
        timezone: None,
        identity,
        provider: Provider::Google,
        remote_account: Some(remote_name.to_string()),
        remote_calendar_id: Some(entry.id.clone()),
        two_way: entry.writable(),
        send_via: cfg.scheduling.send_via,
        is_default_for_identity: false,
    })
}

/// Apply one calendar-list lookup: record every newly discovered calendar
/// in the store and return the mirror configs of all discovered ones.
fn apply_discovery(
    cfg: &JadavConfig,
    store: &Store,
    remote_name: &str,
    entries: &[GCalendarListEntry],
) -> Result<Vec<CalendarMirrorCfg>> {
    let existing = store.list_calendars()?;
    let mut taken: HashSet<String> = existing.iter().map(|c| c.slug.clone()).collect();
    for entry in entries {
        // Already known under some slug (configured or discovered before).
        if existing.iter().any(|c| {
            c.remote_account.as_deref() == Some(remote_name)
                && c.remote_calendar_id.as_deref() == Some(entry.id.as_str())
        }) {
            continue;
        }
        if let Some(spec) = discovered_spec(cfg, remote_name, entry, &taken) {
            taken.insert(spec.slug.clone());
            store.upsert_discovered_calendar(&spec)?;
        }
    }
    discovered_mirror_cfgs(cfg, store, remote_name)
}

/// Build the per-calendar provider factory for one remote. Providers are
/// created on the mirror thread; Google calendars of one remote share a
/// single token holder.
/// The lazily-built, shared Google token holder of one remote: every
/// calendar's provider and the calendar-list discovery refresh through
/// the same `GoogleAuth`, so a remote refreshes its access token once.
type SharedGoogleAuth = Arc<Mutex<Option<Arc<Mutex<google::GoogleAuth>>>>>;

fn shared_google_auth(
    shared: &SharedGoogleAuth,
    store_path: &Path,
    remote_name: &str,
    client_id: &str,
    secret: &str,
) -> Result<Arc<Mutex<google::GoogleAuth>>> {
    let mut slot = shared
        .lock()
        .map_err(|_| anyhow::anyhow!("auth slot poisoned"))?;
    if slot.is_none() {
        let store = Store::open(store_path)?;
        let a = google::GoogleAuth::from_store(&store, store_path, remote_name, client_id, secret)?;
        *slot = Some(Arc::new(Mutex::new(a)));
    }
    Ok(slot.as_ref().cloned().expect("just set"))
}

fn provider_factory(
    cfg: &JadavConfig,
    remote_name: &str,
) -> Result<(ProviderFactory, Option<Discoverer>)> {
    let remote = cfg
        .remotes
        .get(remote_name)
        .with_context(|| format!("no remote {:?}", remote_name))?
        .clone();
    let store_path = cfg.store.clone();
    let name = remote_name.to_string();
    match remote.kind {
        RemoteKind::Google => {
            let (client_id, secret) = google_oauth_secret(cfg)?;
            let shared: SharedGoogleAuth = Arc::new(Mutex::new(None));
            let discoverer: Option<Discoverer> = if remote.mirror_all {
                let cfg = cfg.clone();
                let shared = Arc::clone(&shared);
                let store_path = store_path.clone();
                let name = name.clone();
                let client_id = client_id.clone();
                let secret = secret.clone();
                Some(Box::new(move |store: &Store| {
                    let auth =
                        shared_google_auth(&shared, &store_path, &name, &client_id, &secret)?;
                    let mut client = google::GoogleClient::new(auth);
                    let entries = google::list_calendars(&mut client)?;
                    apply_discovery(&cfg, store, &name, &entries)
                }))
            } else {
                None
            };
            let factory: ProviderFactory = Box::new(move |cal: &CalendarMirrorCfg| {
                let auth = shared_google_auth(&shared, &store_path, &name, &client_id, &secret)?;
                let client = google::GoogleClient::new(auth);
                Ok(Box::new(google::GoogleRest::new(
                    client,
                    &cal.remote_calendar,
                    &cal.identity,
                    cal.send_via,
                )) as Box<dyn mirror::RemoteCalendar>)
            });
            Ok((factory, discoverer))
        }
        RemoteKind::Caldav => {
            let url = remote.url.clone().context("caldav remote needs `url`")?;
            let login = remote.login.clone().unwrap_or_default();
            let auth = remote.auth.clone();
            let factory: ProviderFactory = Box::new(move |cal: &CalendarMirrorCfg| {
                let password = match &auth {
                    Some(a) => a.resolve_password()?,
                    None => String::new(),
                };
                let client = CalDavClient::new(&url, &login, &password)?;
                // `remote_calendar` is either a full collection URL or a
                // display name to discover.
                let collection = if cal.remote_calendar.starts_with("http://")
                    || cal.remote_calendar.starts_with("https://")
                {
                    cal.remote_calendar.clone()
                } else {
                    let found = client
                        .discover_calendars()
                        .map_err(|e| anyhow::anyhow!("discovering calendars on {}: {}", url, e))?;
                    found
                        .iter()
                        .find(|c| c.display_name == cal.remote_calendar)
                        .map(|c| c.url.clone())
                        .with_context(|| {
                            format!(
                                "remote has no calendar named {:?} (found {:?})",
                                cal.remote_calendar,
                                found
                                    .iter()
                                    .map(|c| c.display_name.as_str())
                                    .collect::<Vec<_>>()
                            )
                        })?
                };
                // Re-base the client on the collection's own host: some
                // servers (iCloud) put the calendar home on another host.
                let client = CalDavClient::new(&collection, &login, &password)?;
                Ok(Box::new(caldav_remote::CalDavRemote::new(
                    client,
                    &collection,
                    &cal.identity,
                    &cal.slug,
                    store_path.clone(),
                    cal.send_via,
                )) as Box<dyn mirror::RemoteCalendar>)
            });
            Ok((factory, None))
        }
    }
}

fn mirror_run_once(config_path: &Path, remote_name: &str) -> Result<()> {
    let cfg = load(config_path)?;
    let store = open_store(&cfg)?;
    let (factory, mut discover) = provider_factory(&cfg, remote_name)?;
    let mut cals = all_mirror_cfgs(&cfg, &store, remote_name)?;
    if let Some(d) = discover.as_mut() {
        for extra in d(&store)? {
            if !cals.iter().any(|c| c.slug == extra.slug) {
                println!("discovered {} ({})", extra.slug, extra.remote_calendar);
                cals.push(extra);
            }
        }
    }
    if cals.is_empty() {
        bail!("no calendars are mirrored from remote {:?}", remote_name);
    }
    let log = |msg: &str| eprintln!("jadav: mirror[{}] {}", remote_name, msg);
    for cal in &cals {
        let mut provider = factory(cal)?;
        match mirror::sync_calendar_once(&store, provider.as_mut(), cal, &log) {
            Ok(stats) => println!(
                "{}: pulled {} pushed {} deleted(local {} remote {}) reverted {} conflicts {} echoes {}",
                cal.slug,
                stats.pulled,
                stats.pushed,
                stats.deleted_local,
                stats.deleted_remote,
                stats.reverted,
                stats.conflicts,
                stats.echoes
            ),
            Err(e) => {
                store.set_remote_status(
                    remote_name,
                    matches!(e, mirror::RemoteError::NeedsReauth),
                    Some(&e.to_string()),
                )?;
                bail!("{}: {}", cal.slug, e)
            }
        }
    }
    store.set_remote_status(remote_name, false, None)?;
    Ok(())
}

/// `jadav mirror refresh <remote>`: forget sync tokens and recorded remote
/// versions for every calendar of the remote so the running daemon's next
/// pass (within its poll interval) re-applies whatever the mirror now
/// renders differently, e.g. after a mapping fix.
fn mirror_refresh(config_path: &Path, remote_name: &str) -> Result<()> {
    let cfg = load(config_path)?;
    let store = open_store(&cfg)?;
    let cals = all_mirror_cfgs(&cfg, &store, remote_name)?;
    if cals.is_empty() {
        bail!("no calendars are mirrored from remote {:?}", remote_name);
    }
    for cal in &cals {
        let n = store.force_mirror_refresh(&cal.slug)?;
        println!(
            "{}: {} objects will be re-compared on the next pass",
            cal.slug, n
        );
    }
    println!(
        "the running jadav picks this up within its poll interval; `jadav mirror run-once {}` does it now",
        remote_name
    );
    Ok(())
}

fn load(config_path: &std::path::Path) -> Result<JadavConfig> {
    let cfg = JadavConfig::load_from(config_path)?;
    let warnings = cfg.validate()?;
    for w in warnings {
        eprintln!("jadav: config warning: {}", w);
    }
    Ok(cfg)
}

/// Turn config calendars into store specs (identities resolved, per-calendar
/// `send_via` defaulted from `scheduling`, `two_way` forced on for native).
pub fn calendar_specs(cfg: &JadavConfig) -> Result<Vec<CalendarSpec>> {
    cfg.calendars
        .iter()
        .map(|cal: &CalendarConfig| {
            Ok(CalendarSpec {
                slug: cal.slug.clone(),
                display_name: cal.name.clone(),
                description: cal.description.clone(),
                color: cal.color.clone(),
                timezone: cal.timezone.clone(),
                identity: cfg
                    .calendar_identity(cal)
                    .with_context(|| format!("calendar {} has no identity", cal.slug))?,
                provider: cal.provider,
                remote_account: cal.remote.clone(),
                remote_calendar_id: cal.remote_calendar.clone(),
                two_way: cal.provider == Provider::Native || cal.two_way,
                send_via: cal.send_via.unwrap_or(cfg.scheduling.send_via),
                is_default_for_identity: cal.default_for_identity,
            })
        })
        .collect()
}

fn open_store(cfg: &JadavConfig) -> Result<Store> {
    let store = Store::open(&cfg.store)?;
    store.set_principal(&cfg.principal.login, &cfg.identities())?;
    for w in store.reconcile_calendars(&calendar_specs(cfg)?)? {
        eprintln!("jadav: {}", w);
    }
    Ok(store)
}

fn serve(config_path: &std::path::Path) -> Result<()> {
    let cfg = load(config_path)?;
    let store = open_store(&cfg)?;
    let retention = i64::from(cfg.sync_log_retention_days) * 86_400;
    let report = store.compact_change_log(retention, chrono::Utc::now().timestamp())?;
    if report.deleted_rows > 0 {
        eprintln!(
            "jadav: compacted {} change-log rows across {} calendars",
            report.deleted_rows, report.calendars_touched
        );
    }
    drop(store);

    let password = cfg
        .principal
        .auth
        .resolve_password()
        .context("resolving jadav.principal.auth")?;

    // Mirror threads: one per remote that has calendars, woken by client
    // writes to those calendars through the hub.
    let mut hub = MirrorHub::default();
    let mut mirror_threads = Vec::new();
    let mut mirror_plan: Vec<(String, Vec<CalendarMirrorCfg>, Arc<MirrorControl>, Duration)> =
        Vec::new();
    let store = Store::open(&cfg.store)?;
    for (remote_name, remote_cfg) in &cfg.remotes {
        let cals = all_mirror_cfgs(&cfg, &store, remote_name)?;
        if cals.is_empty() && !remote_cfg.mirror_all {
            continue;
        }
        let control = Arc::new(MirrorControl::new());
        let slugs: Vec<String> = cals.iter().map(|c| c.slug.clone()).collect();
        hub.register(remote_name, Arc::clone(&control), &slugs);
        mirror_plan.push((
            remote_name.clone(),
            cals,
            control,
            Duration::from_secs(remote_cfg.poll_interval_secs.max(15)),
        ));
    }
    drop(store);
    let hub = Arc::new(hub);

    let mut state = server::ServerState::new(
        cfg.store.clone(),
        &cfg.principal.login,
        &password,
        cfg.identities(),
    );
    {
        let hub = Arc::clone(&hub);
        state.on_client_write = Some(Arc::new(move |outcome: &store::WriteOutcome| {
            hub.wake_for_calendar(&outcome.calendar.slug);
        }));
    }
    let scheduling = Arc::new(scheduling_runtime(&cfg));
    state.scheduling = Some(Arc::clone(&scheduling));
    let state = Arc::new(state);
    install_signal_handlers();
    let (handle, addr) = server::spawn(&cfg.listen, Arc::clone(&state))?;

    // iMIP: the outbound SMTP worker and the inbound IMAP watcher, both
    // only when our own mail server is configured.
    let mut mail_threads = Vec::new();
    if let Some(mail) = &cfg.mail {
        let sender: Box<dyn imip::MailSender> = Box::new(imip::SmtpSender::new(mail.smtp.clone()));
        let store_path = cfg.store.clone();
        let wake = Arc::clone(&scheduling.outbound_wake);
        let max_age = scheduling.retry_max_age_secs;
        let shutdown = Arc::clone(&state.shutdown);
        mail_threads.push(
            std::thread::Builder::new()
                .name("jadav-imip-out".into())
                .spawn(move || {
                    imip::run_outbound_worker(store_path, sender, wake, max_age, shutdown)
                })?,
        );
        let inbound = imip::InboundConfig::from_mail_config(mail);
        eprintln!(
            "jadav: watching {} on {} for iTIP mail; sending via {}",
            inbound.folders.join(", "),
            mail.imap.host,
            mail.smtp.host
        );
        let store_path = cfg.store.clone();
        let runtime = Arc::clone(&scheduling);
        let shutdown = Arc::clone(&state.shutdown);
        mail_threads.push(
            std::thread::Builder::new()
                .name("jadav-imip-in".into())
                .spawn(move || imip::run_inbound_watcher(inbound, store_path, runtime, shutdown))?,
        );
    } else {
        eprintln!("jadav: no mail: section — scheduling stores documents as received, no iMIP");
    }
    for (remote_name, cals, control, interval) in mirror_plan {
        let (factory, discover) = provider_factory(&cfg, &remote_name)?;
        eprintln!(
            "jadav: mirroring {} calendar(s) from remote {} every {}s{}",
            cals.len(),
            remote_name,
            interval.as_secs(),
            if discover.is_some() {
                " (+ every calendar on the account, re-listed hourly)"
            } else {
                ""
            }
        );
        mirror_threads.push(mirror::spawn_mirror_thread(
            remote_name,
            cals,
            cfg.store.clone(),
            interval,
            factory,
            discover,
            control,
        ));
    }
    eprintln!(
        "jadav: listening on {} (principal {}, {} calendars, store {})",
        addr,
        cfg.principal.login,
        cfg.calendars.len(),
        cfg.store.display()
    );

    let mut last_compaction = Instant::now();
    loop {
        if TERM_RECEIVED.load(Ordering::SeqCst) {
            eprintln!("jadav: shutting down");
            state.shutdown.store(true, Ordering::Relaxed);
            hub.shutdown_all();
            scheduling.wake_outbound();
            break;
        }
        if HUP_RECEIVED.swap(false, Ordering::SeqCst) {
            match cfg.principal.auth.resolve_password() {
                Ok(pw) => {
                    if let Ok(mut creds) = state.creds.write() {
                        *creds = server::Credentials::new(&cfg.principal.login, &pw);
                    }
                    eprintln!("jadav: SIGHUP: credentials reloaded");
                }
                Err(e) => eprintln!("jadav: SIGHUP: could not re-resolve password: {:#}", e),
            }
        }
        if last_compaction.elapsed() >= COMPACTION_INTERVAL {
            last_compaction = Instant::now();
            if let Ok(store) = Store::open(&cfg.store)
                && let Ok(report) =
                    store.compact_change_log(retention, chrono::Utc::now().timestamp())
                && report.deleted_rows > 0
            {
                eprintln!(
                    "jadav: compacted {} change-log rows across {} calendars",
                    report.deleted_rows, report.calendars_touched
                );
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = handle.join();
    for t in mirror_threads {
        let _ = t.join();
    }
    for t in mail_threads {
        let _ = t.join();
    }
    Ok(())
}

/// Resolve everything the scheduling paths need from config, once.
fn scheduling_runtime(cfg: &JadavConfig) -> imip::SchedulingRuntime {
    let identities = cfg.identities();
    let identity_names = identities
        .iter()
        .filter_map(|id| cfg.identity_display_name(id).map(|n| (id.clone(), n)))
        .collect();
    let message_id_domain = cfg
        .principal
        .login
        .rsplit_once('@')
        .map(|(_, d)| d.to_string())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "jadav.invalid".to_string());
    imip::SchedulingRuntime {
        identities,
        identity_names,
        significant_properties: cfg.scheduling.significant_properties.clone(),
        prodid: itip::DEFAULT_PRODID.to_string(),
        templates: imip::SubjectTemplates::default(),
        default_timezone: cfg.scheduling.default_timezone.clone(),
        message_id_domain,
        inbox_for_mirrored: cfg.scheduling.inbox_for_mirrored,
        retry_max_age_secs: i64::from(cfg.scheduling.retry_max_age_hours) * 3600,
        mail_enabled: cfg.mail.is_some(),
        outbound_wake: Arc::new((Mutex::new(false), std::sync::Condvar::new())),
    }
}

fn check_config(config_path: &std::path::Path) -> Result<()> {
    let cfg = load(config_path)?;
    println!("config:    {}", config_path.display());
    println!("listen:    {}", cfg.listen);
    println!("store:     {}", cfg.store.display());
    println!("principal: {}", cfg.principal.login);
    for (i, id) in cfg.identities().iter().enumerate() {
        println!(
            "identity:  {}{}",
            id,
            if i == 0 { " (primary)" } else { "" }
        );
    }
    for spec in calendar_specs(&cfg)? {
        println!(
            "calendar:  /dav/calendars/{}/{}/  \"{}\"  identity={} provider={}{}{}{}",
            cfg.principal.login,
            spec.slug,
            spec.display_name,
            spec.identity,
            spec.provider.as_str(),
            spec.remote_account
                .as_ref()
                .map(|r| format!(" remote={}", r))
                .unwrap_or_default(),
            if spec.provider != Provider::Native && !spec.two_way {
                " read-only"
            } else {
                ""
            },
            if spec.is_default_for_identity {
                " default"
            } else {
                ""
            },
        );
    }
    for (name, remote) in &cfg.remotes {
        println!(
            "remote:    {} kind={:?} user={} poll={}s{}",
            name,
            remote.kind,
            remote.user.as_deref().unwrap_or("-"),
            remote.poll_interval_secs,
            if remote.mirror_all {
                format!(
                    " mirror_all (slugs {}-*)",
                    remote
                        .slug_prefix
                        .clone()
                        .unwrap_or_else(|| format!("g-{}", slugify(name)))
                )
            } else {
                String::new()
            }
        );
    }
    match &cfg.mail {
        Some(mail) => {
            let inbound = imip::InboundConfig::from_mail_config(mail);
            println!(
                "mail:      imap {}:{} as {} folders=[{}] since={}d poll={}s",
                mail.imap.host,
                mail.imap.port,
                mail.imap.login,
                inbound.folders.join(", "),
                inbound.since_days,
                inbound.poll_interval.as_secs()
            );
            println!(
                "mail:      smtp {}:{} as {}",
                mail.smtp.host, mail.smtp.port, mail.smtp.login
            );
            let mut keys: Vec<(&String, &String)> = inbound.keyword_identities.iter().collect();
            keys.sort();
            for (keyword, identity) in keys {
                println!("keyword:   {} -> {}", keyword, identity);
            }
        }
        None => println!("mail:      (none) — no iMIP; client writes are stored as received"),
    }
    println!(
        "scheduling: send_via={:?} significant=[{}] inbox_for_mirrored={} retry_max_age={}h tz={}",
        cfg.scheduling.send_via,
        cfg.scheduling.significant_properties.join(" "),
        cfg.scheduling.inbox_for_mirrored,
        cfg.scheduling.retry_max_age_hours,
        cfg.scheduling.default_timezone
    );
    println!("ok");
    Ok(())
}

fn health(config_path: &std::path::Path) -> Result<()> {
    let cfg = JadavConfig::load_from(config_path)?;
    let port = cfg
        .listen
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .context("cannot read a port out of jadav.listen")?;
    let url = crate::httpc::HttpUrl::parse(&format!("http://127.0.0.1:{}/healthz", port))?;
    let resp = crate::httpc::send("GET", &url, &[], None, Duration::from_secs(5))?;
    if resp.status == 200 {
        println!("ok");
        Ok(())
    } else {
        bail!("healthz returned HTTP {}", resp.status)
    }
}

/// One-off migration: copy every object of a remote CalDAV calendar into
/// a local calendar, keeping each object's href basename. Idempotent —
/// re-running overwrites by href. Writes carry [`Origin::Mirror`] so a
/// later scheduling engine never treats them as client edits.
fn import_caldav(
    config_path: &std::path::Path,
    base_url: &str,
    slug: &str,
    remote_calendar: Option<&str>,
    login: &str,
    auth: &AuthConfig,
) -> Result<()> {
    let cfg = load(config_path)?;
    let store = open_store(&cfg)?;
    let target = store.get_calendar(slug)?.with_context(|| {
        format!(
            "no local calendar with slug {:?} (configure it first)",
            slug
        )
    })?;
    let password = auth.resolve_password()?;
    let client = CalDavClient::new(base_url, login, &password)?;
    let discovered = client
        .discover_calendars()
        .map_err(|e| anyhow::anyhow!("discovering remote calendars: {}", e))?;
    let remote = match remote_calendar {
        Some(name) => discovered
            .iter()
            .find(|c| c.display_name == name)
            .with_context(|| {
                format!(
                    "remote has no calendar named {:?}; found: {:?}",
                    name,
                    discovered
                        .iter()
                        .map(|c| c.display_name.as_str())
                        .collect::<Vec<_>>()
                )
            })?,
        None => match discovered.as_slice() {
            [one] => one,
            _ => bail!(
                "remote has {} calendars; pick one with --remote-calendar: {:?}",
                discovered.len(),
                discovered
                    .iter()
                    .map(|c| c.display_name.as_str())
                    .collect::<Vec<_>>()
            ),
        },
    };
    eprintln!(
        "jadav: importing {:?} ({}) into /dav/calendars/{}/{}/",
        remote.display_name, remote.url, cfg.principal.login, target.slug
    );
    let events = client
        .list_all_events(&remote.url)
        .map_err(|e| anyhow::anyhow!("listing remote events: {}", e))?;
    let (mut created, mut updated, mut failed) = (0usize, 0usize, 0usize);
    for ev in &events {
        let ics = match &ev.calendar_data {
            Some(d) => d.clone(),
            None => match client.get_event(&ev.href) {
                Ok((_, body)) => body,
                Err(e) => {
                    eprintln!("jadav:   {}: GET failed: {}", ev.href, e);
                    failed += 1;
                    continue;
                }
            },
        };
        let name = percent_encoding::percent_decode_str(ev.href.rsplit('/').next().unwrap_or(""))
            .decode_utf8_lossy()
            .into_owned();
        if name.is_empty() {
            failed += 1;
            continue;
        }
        match store.put_object(
            &target.slug,
            &name,
            &ics,
            Precondition::None,
            Origin::Mirror,
        )? {
            Ok(outcome) => {
                if outcome.old.is_some() {
                    updated += 1
                } else {
                    created += 1
                }
            }
            Err(e) => {
                eprintln!("jadav:   {}: rejected: {:?}", ev.href, e);
                failed += 1;
            }
        }
    }
    println!(
        "imported {} objects: {} created, {} updated, {} failed",
        events.len(),
        created,
        updated,
        failed
    );
    if failed > 0 {
        bail!("{} objects could not be imported", failed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn cli_defaults_to_serve_and_reads_config_flag() {
        let cli = parse_cli(&args("")).unwrap();
        assert_eq!(cli.command, Command::Serve);
        assert!(cli.config.is_none());
        let cli = parse_cli(&args("--config /etc/jadav/config.yaml check-config")).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/etc/jadav/config.yaml")));
        assert_eq!(cli.command, Command::CheckConfig);
        assert_eq!(parse_cli(&args("health")).unwrap().command, Command::Health);
        assert!(parse_cli(&args("frobnicate")).is_err());
    }

    #[test]
    fn cli_import_caldav_arguments() {
        let cli = parse_cli(&args(
            "import-caldav https://dav.example/dav/ --calendar personal --remote-calendar Personal --login t@x --password-command pass_show",
        ))
        .unwrap();
        assert_eq!(
            cli.command,
            Command::ImportCaldav {
                base_url: "https://dav.example/dav/".to_string(),
                slug: "personal".to_string(),
                remote_calendar: Some("Personal".to_string()),
                login: "t@x".to_string(),
                password: None,
                password_command: Some("pass_show".to_string()),
            }
        );
        assert!(parse_cli(&args("import-caldav --calendar personal --login t")).is_err());
    }

    #[test]
    fn calendar_specs_resolve_identity_send_via_and_two_way() {
        let yaml = "jadav:\n  store: /tmp/x.db\n  principal:\n    login: a@example.com\n    auth: {type: password, value: p}\n    identities: [a@example.com, b@example.com]\n  google_oauth: {client_id: c, client_secret: {type: password, value: s}}\n  remotes:\n    g: {kind: google, user: b@example.com}\n  scheduling: {send_via: provider}\n  calendars:\n    - {slug: personal, name: Personal}\n    - {slug: g-b, name: B, identity: b@example.com, provider: google, remote: g, two_way: false, send_via: smtp}\n";
        let file: serde_yml::Value = serde_yml::from_str(yaml).unwrap();
        let cfg: JadavConfig = serde_yml::from_value(file["jadav"].clone()).unwrap();
        let specs = calendar_specs(&cfg).unwrap();
        assert_eq!(specs[0].identity, "a@example.com");
        assert!(specs[0].two_way);
        assert_eq!(specs[0].send_via, store::SendVia::Provider);
        assert_eq!(specs[1].identity, "b@example.com");
        assert!(!specs[1].two_way);
        assert_eq!(specs[1].send_via, store::SendVia::Smtp);
        assert_eq!(specs[1].remote_account.as_deref(), Some("g"));
    }

    fn mirror_all_cfg(extra_calendars: &str) -> JadavConfig {
        let yaml = format!(
            "jadav:\n  store: /tmp/x.db\n  principal:\n    login: a@example.com\n    auth: {{type: password, value: p}}\n    identities: [a@example.com, w@example.com]\n  google_oauth: {{client_id: c, client_secret: {{type: password, value: s}}}}\n  remotes:\n    work: {{kind: google, user: W@example.com, mirror_all: true, skip_calendars: [\"Holidays in Hungary\"]}}\n  calendars:\n    - {{slug: personal, name: Personal}}\n    - {{slug: g-work, name: Work, identity: w@example.com, provider: google, remote: work, two_way: true}}\n{}",
            extra_calendars
        );
        let file: serde_yml::Value = serde_yml::from_str(&yaml).unwrap();
        serde_yml::from_value(file["jadav"].clone()).unwrap()
    }

    fn entry(id: &str, summary: &str, role: &str) -> GCalendarListEntry {
        GCalendarListEntry {
            id: id.to_string(),
            summary: Some(summary.to_string()),
            access_role: Some(role.to_string()),
            background_color: Some("#9fe1e7".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn slugify_is_lowercase_ascii_with_single_dashes() {
        assert_eq!(slugify("Anniversaries"), "anniversaries");
        assert_eq!(slugify("Holidays in Hungary!"), "holidays-in-hungary");
        assert_eq!(slugify("  Été / Ünnep  "), "ete-unnep");
        assert_eq!(
            slugify("Ünnepnapok Magyarországon"),
            "unnepnapok-magyarorszagon"
        );
        assert_eq!(slugify("Straße & Øre"), "strase-ore");
        assert_eq!(slugify("***"), "calendar");
        assert!(slugify(&"x".repeat(100)).len() <= 40);
    }

    #[test]
    fn discovered_spec_maps_google_calendars_and_skips_what_it_should() {
        let cfg = mirror_all_cfg("");
        let taken: HashSet<String> = ["g-work".to_string(), "personal".to_string()].into();

        // The primary is already configured as g-work: left alone.
        let mut primary = entry("w@example.com", "W", "owner");
        primary.primary = Some(true);
        assert!(discovered_spec(&cfg, "work", &primary, &taken).is_none());

        // A shared read-only calendar becomes a read-only mirror.
        let anniv = entry("abc@group.calendar.google.com", "Anniversaries", "reader");
        let spec = discovered_spec(&cfg, "work", &anniv, &taken).unwrap();
        assert_eq!(spec.slug, "g-work-anniversaries");
        assert_eq!(spec.display_name, "Anniversaries (work)");
        assert_eq!(spec.identity, "w@example.com");
        assert_eq!(spec.provider, Provider::Google);
        assert_eq!(spec.remote_account.as_deref(), Some("work"));
        assert_eq!(
            spec.remote_calendar_id.as_deref(),
            Some("abc@group.calendar.google.com")
        );
        assert!(!spec.two_way);
        assert_eq!(spec.color.as_deref(), Some("#9fe1e7"));
        assert!(!spec.is_default_for_identity);

        // Writer access: two-way.
        let team = entry("team@group.calendar.google.com", "Team", "writer");
        assert!(
            discovered_spec(&cfg, "work", &team, &taken)
                .unwrap()
                .two_way
        );

        // Free/busy only, deleted, hidden and skipped-by-name are not mirrored.
        assert!(
            discovered_spec(&cfg, "work", &entry("fb@x", "FB", "freeBusyReader"), &taken).is_none()
        );
        let mut gone = entry("gone@x", "Gone", "owner");
        gone.deleted = Some(true);
        assert!(discovered_spec(&cfg, "work", &gone, &taken).is_none());
        let mut hidden = entry("hid@x", "Hidden", "owner");
        hidden.hidden = Some(true);
        assert!(discovered_spec(&cfg, "work", &hidden, &taken).is_none());
        let holidays = entry(
            "hu.hungarian#holiday@group.v.calendar.google.com",
            "Holidays in Hungary",
            "reader",
        );
        assert!(discovered_spec(&cfg, "work", &holidays, &taken).is_none());

        // A slug clash gets a stable suffix from the remote id.
        let taken2: HashSet<String> = ["g-work-anniversaries".to_string()].into();
        let clash = discovered_spec(&cfg, "work", &anniv, &taken2).unwrap();
        assert!(clash.slug.starts_with("g-work-anniversaries-"));
        assert_eq!(clash.slug.len(), "g-work-anniversaries-".len() + 6);
        assert_eq!(
            discovered_spec(&cfg, "work", &anniv, &taken2).unwrap().slug,
            clash.slug
        );

        // An explicitly configured non-primary calendar is covered too.
        let cfg2 = mirror_all_cfg(
            "    - {slug: my-anniv, name: A, identity: w@example.com, provider: google, remote: work, remote_calendar: abc@group.calendar.google.com}\n",
        );
        assert!(discovered_spec(&cfg2, "work", &anniv, &taken).is_none());
    }

    #[test]
    fn apply_discovery_records_new_calendars_once_and_returns_all_discovered() {
        let dir = std::env::temp_dir().join(format!("jadav-discover-{}", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        let store = Store::open(&dir).unwrap();
        let cfg = mirror_all_cfg("");
        store
            .set_principal("a@example.com", &cfg.identities())
            .unwrap();
        store
            .reconcile_calendars(&calendar_specs(&cfg).unwrap())
            .unwrap();
        let entries = vec![
            entry("abc@group.calendar.google.com", "Anniversaries", "reader"),
            entry("team@group.calendar.google.com", "Team", "writer"),
        ];
        let first = apply_discovery(&cfg, &store, "work", &entries).unwrap();
        let mut slugs: Vec<&str> = first.iter().map(|c| c.slug.as_str()).collect();
        slugs.sort();
        assert_eq!(slugs, vec!["g-work-anniversaries", "g-work-team"]);
        // Renamed on Google: the slug stays, nothing is duplicated.
        let renamed = vec![entry(
            "abc@group.calendar.google.com",
            "Anniversaries & more",
            "reader",
        )];
        let second = apply_discovery(&cfg, &store, "work", &renamed).unwrap();
        assert_eq!(second.len(), 2);
        // Reconciliation from config does not complain about discovered rows.
        let warnings = store
            .reconcile_calendars(&calendar_specs(&cfg).unwrap())
            .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(all_mirror_cfgs(&cfg, &store, "work").unwrap().len(), 3);
        let _ = std::fs::remove_file(&dir);
        let _ = std::fs::remove_file(format!("{}-wal", dir.display()));
        let _ = std::fs::remove_file(format!("{}-shm", dir.display()));
    }
}
