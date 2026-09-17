//! `jamaild` — the jamail background daemon.
//!
//! Owns every configured account's IMAP connection/sync loop, writes to the
//! shared local cache, and delivers desktop notifications. Talks to the
//! `jamail` foreground client over the IPC protocol documented in
//! `jamail::ipc`. See that module and `jamail::daemon` for the full
//! protocol/lifecycle/compatibility contract.
//!
//! Designed to run under `systemd --user` (see `contrib/systemd/jamaild.service`)
//! but works identically started directly from a shell — `jamail` will even
//! auto-spawn one if none is reachable (see `jamail::daemon::try_autostart`).
//!
//! ```text
//! jamaild [serve]                        run the daemon (the default)
//! jamaild google-auth <account> [--print]  authorize Google Calendar once
//! jamaild caldav-check [<account>]       list the calendars sync can see
//! ```
//!
//! The two setup commands exist because a CalDAV account can now
//! authenticate with OAuth (`caldav.oauth`, which is the only thing Google
//! accepts), and OAuth needs one human-in-a-browser step before a daemon
//! can run unattended: `google-auth` is that step, `caldav-check` proves
//! the result works without starting a TUI. Neither talks to a running
//! `jamaild` — they read the same config and go straight to the server —
//! so both are safe to run while the daemon is up.
//!
//! Exits non-zero with a message on stderr if it can't load config, can't
//! bind its socket, or another `jamaild` is already running.

use anyhow::{Context, Result, bail};
use jamail::config::{CalDavConfig, JamailAccount, JamailConfig};
use jamail::{calsync, goauth};

const USAGE: &str = "\
usage: jamaild [serve]
       jamaild google-auth <account> [--print]
       jamaild caldav-check [<account>]
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None | Some("serve") => jamail::daemon::run(),
        Some("-h") | Some("--help") => {
            print!("{}", USAGE);
            return;
        }
        Some("google-auth") => google_auth(&args[1..]),
        Some("caldav-check") => caldav_check(args.get(1).map(String::as_str)),
        Some(other) => Err(anyhow::anyhow!("unknown command {:?}\n{}", other, USAGE)),
    };
    if let Err(err) = result {
        eprintln!("jamaild: {:?}", err);
        std::process::exit(1);
    }
}

/// The named account, or the single/default one when no name is given.
fn account_of<'a>(
    config: &'a JamailConfig,
    name: Option<&str>,
) -> Result<(&'a str, &'a JamailAccount)> {
    match name {
        Some(n) => config
            .accounts
            .get_key_value(n)
            .map(|(k, v)| (k.as_str(), v))
            .with_context(|| {
                format!(
                    "no account {:?} in the config (accounts: {})",
                    n,
                    config
                        .accounts
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }),
        None => config.default_account(),
    }
}

fn caldav_of<'a>(name: &str, account: &'a JamailAccount) -> Result<&'a CalDavConfig> {
    account
        .caldav
        .as_ref()
        .with_context(|| format!("account {:?} has no `caldav:` block configured", name))
}

/// `jamaild google-auth <account>`: the one interactive step of Google
/// CalDAV setup. Reads `caldav.oauth.client_id`/`client_secret` from the
/// account, walks the PKCE flow, and stores the refresh token it gets —
/// by default in a `0600` file (so the secret never lands in the
/// terminal's scrollback), or on stdout with `--print` for piping into a
/// password manager.
fn google_auth(args: &[String]) -> Result<()> {
    let mut account_name = None;
    let mut print = false;
    for arg in args {
        match arg.as_str() {
            "--print" => print = true,
            other if other.starts_with('-') => bail!("unknown option {:?}\n{}", other, USAGE),
            other if account_name.is_none() => account_name = Some(other.to_string()),
            other => bail!("unexpected argument {:?}\n{}", other, USAGE),
        }
    }

    let config = JamailConfig::load()?;
    let (name, account) = account_of(&config, account_name.as_deref())?;
    let caldav = caldav_of(name, account)?;
    let oauth = caldav.oauth.as_ref().with_context(|| {
        format!(
            "account {:?} has no `caldav.oauth` block — add client_id and client_secret \
             first (see README: \"Google Calendar in jamaild\")",
            name
        )
    })?;
    let client_secret = oauth
        .client_secret
        .resolve_password()
        .context("resolving caldav.oauth.client_secret")?;
    // Google shows this address on the consent screen and refuses a token
    // for a different one, which turns a wrong-account sign-in into an
    // error there instead of a confusing empty calendar list here.
    let login_hint = if caldav.login.contains('@') {
        &caldav.login
    } else {
        &account.email
    };

    let grant = goauth::authorize_interactive(&oauth.client_id, &client_secret, login_hint)?;
    let refresh_token = grant
        .refresh_token
        .context("Google returned no refresh token")?;

    println!();
    if print {
        println!(
            "refresh token for account {:?} (store it somewhere safe):",
            name
        );
        println!("{}", refresh_token);
        println!();
        println!("Then point the config at it, e.g. via a password manager:");
        println!("    accounts.{}.caldav.oauth.refresh_token:", name);
        println!("      type: command");
        println!("      value: pass show google/{}/refresh_token", name);
    } else {
        let path = goauth::refresh_token_path(name)?;
        goauth::write_refresh_token(&path, &refresh_token)?;
        println!(
            "Stored the refresh token in {} (mode 0600).",
            path.display()
        );
        println!();
        println!("Add this to the account's caldav.oauth block:");
        println!("      refresh_token:");
        println!("        type: command");
        println!("        value: cat {}", path.display());
        println!();
        println!("(or re-run with --print to handle the token yourself)");
    }
    println!();
    println!("Then check it: jamaild caldav-check {}", name);
    Ok(())
}

/// `jamaild caldav-check [account]`: resolve the account's CalDAV
/// credentials exactly the way the sync thread does and list what
/// discovery finds, so a misconfigured URL or a stale token is one command
/// away instead of a line in the journal.
fn caldav_check(account_name: Option<&str>) -> Result<()> {
    let config = JamailConfig::load()?;
    let targets: Vec<(String, JamailAccount)> = match account_name {
        Some(_) => {
            let (name, account) = account_of(&config, account_name)?;
            vec![(name.to_string(), account.clone())]
        }
        None => config
            .accounts
            .iter()
            .filter(|(_, a)| a.caldav.is_some())
            .map(|(n, a)| (n.clone(), a.clone()))
            .collect(),
    };
    if targets.is_empty() {
        bail!("no account in the config has a `caldav:` block");
    }

    let mut failures = 0;
    for (name, account) in &targets {
        let caldav = caldav_of(name, account)?;
        println!("{} — {}", name, caldav.url);
        let scheme = if caldav.oauth.is_some() {
            "OAuth bearer"
        } else {
            "HTTP Basic"
        };
        println!("  auth: {}", scheme);
        match calsync::connect(caldav, &mut None)
            .and_then(|c| c.discover_calendars().map_err(|e| anyhow::anyhow!("{}", e)))
        {
            Ok(calendars) => {
                let selected = caldav.calendars.as_ref();
                for cal in &calendars {
                    let skipped =
                        if selected.is_some_and(|names| !names.contains(&cal.display_name)) {
                            "  (not in caldav.calendars — not synced)"
                        } else {
                            ""
                        };
                    println!("  · {}{}", cal.display_name, skipped);
                    println!("      {}", cal.url);
                }
                println!("  {} calendar(s) discovered", calendars.len());
            }
            Err(e) => {
                failures += 1;
                println!("  FAILED: {:#}", e);
            }
        }
    }
    if failures > 0 {
        bail!("{} of {} account(s) failed", failures, targets.len());
    }
    Ok(())
}
