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

pub mod config;
pub mod server;
pub mod store;
pub mod xml;

use crate::caldav::CalDavClient;
use crate::config::AuthConfig;
use anyhow::{Context, Result, bail};
use config::{CalendarConfig, JadavConfig, Provider};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Reserved for the mirror milestone (`google auth|import-token|calendars`).
    Google(Vec<String>),
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
       jadav [--config <path>] import-caldav <base-url> --calendar <slug> [--remote-calendar <name>]
                               --login <login> (--password <pw> | --password-command <cmd>)
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
        Command::Google(_) => bail!("`jadav google …` arrives with the mirror milestone"),
    }
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
    let state = Arc::new(server::ServerState::new(
        cfg.store.clone(),
        &cfg.principal.login,
        &password,
        cfg.identities(),
    ));
    install_signal_handlers();
    let (handle, addr) = server::spawn(&cfg.listen, Arc::clone(&state))?;
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
    Ok(())
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
            "remote:    {} kind={:?} user={} poll={}s",
            name,
            remote.kind,
            remote.user.as_deref().unwrap_or("-"),
            remote.poll_interval_secs
        );
    }
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
        let yaml = "jadav:\n  store: /tmp/x.db\n  principal:\n    login: a@example.com\n    auth: {type: password, value: p}\n    identities: [a@example.com, b@example.com]\n  remotes:\n    g: {kind: google, user: b@example.com}\n  scheduling: {send_via: provider}\n  calendars:\n    - {slug: personal, name: Personal}\n    - {slug: g-b, name: B, identity: b@example.com, provider: google, remote: g, two_way: false, send_via: smtp}\n";
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
}
