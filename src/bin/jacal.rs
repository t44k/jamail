//! `jacal` — the calendar terminal UI client.
//!
//! Sibling to `jamail`/`jamaild`: reads the shared local SQLite cache
//! directly (calendars/events synced there by `jamaild`'s per-account
//! calendar sync thread — see `jamail::calsync`) and talks to `jamaild`
//! over the same IPC protocol (`jamail::ipc`) to trigger a manual sync
//! (`Request::SyncCalendars`) and to queue create/update/delete writes
//! (`Request::EnqueueCalendarMutation`), mirroring how `jamail` queues
//! Sent/Draft uploads rather than writing to IMAP directly.
//!
//! Only accounts with `caldav` configured are shown; if none are, this
//! binary exits immediately with a clear message rather than opening an
//! empty calendar UI.
//!
//! Calendar alarm notifications are *not* this process's job: `jamaild`
//! runs the alarm clock for every caldav-configured account (see
//! `jamail::calnotify`), so reminders fire whether or not `jacal` is open.

use anyhow::{Context, Result};
use chrono::{Local, TimeZone, Utc};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use jamail::calapp::{CalApp, CalField, CalMode};
use jamail::calendar::{EventEdits, Rsvp};
use jamail::calsync::CalMutation;
use jamail::db::MailDb;
use jamail::{config, daemon, ipc};
use ratatui::prelude::*;
use std::io::{self, IsTerminal};
use std::time::{Duration, Instant};

/// How often to reload calendars/events from the local cache even without
/// an explicit IPC event nudging a reload (covers the case where
/// `jamaild` updated the cache between polls, or `jacal` started before
/// any sync completed).
const CALENDAR_RELOAD_INTERVAL: Duration = Duration::from_secs(20);

fn main() -> Result<()> {
    if !io::stdout().is_terminal() {
        anyhow::bail!("jacal requires an interactive terminal (TTY) to run");
    }

    let config = config::JamailConfig::load().context("Failed to load jamail config")?;
    let daemon_socket_override = config.daemon.as_ref().and_then(|d| d.socket_path.clone());
    let week_start = config
        .jacal
        .as_ref()
        .map(|j| j.week_start)
        .unwrap_or_default();
    let jacal_prefs = config
        .jacal
        .as_ref()
        .map(|j| j.calendars.clone())
        .unwrap_or_default();
    let show_declined = config
        .jacal
        .as_ref()
        .map(|j| j.show_declined)
        .unwrap_or(true);
    let accounts: Vec<(String, config::JamailAccount)> = config.accounts.into_iter().collect();

    let caldav_accounts: Vec<&(String, config::JamailAccount)> = accounts
        .iter()
        .filter(|(_, a)| a.caldav.is_some())
        .collect();
    if caldav_accounts.is_empty() {
        anyhow::bail!(
            "No account in config.yaml has `caldav` configured — jacal has nothing to show. \
             See config.example.yaml for the `caldav:` block."
        );
    }
    let current_account = caldav_accounts[0].0.clone();

    let db = MailDb::open().context("Failed to open mail database")?;

    let mut app = CalApp::new(current_account.clone(), week_start);
    app.identities = own_identities(&caldav_accounts[0].1);
    app.show_declined = show_declined;
    app.set_calendar_prefs(jacal_prefs);
    reload_calendars(&mut app, &db);
    reload_events(&mut app, &db);
    reload_invitations(&mut app, &db);

    let socket_path = ipc::resolve_socket_path(daemon_socket_override.as_deref());
    daemon::try_autostart(&socket_path);
    let ipc_client = ipc::IpcClient::spawn(socket_path);
    // Kick off a sync immediately rather than waiting for the account's
    // next poll interval (config::CalDavConfig::poll_interval_secs).
    ipc_client.send(ipc::Request::SyncCalendars {
        account: current_account.clone(),
    });

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, &mut app, &db, &ipc_client);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(err) = result {
        eprintln!("Error: {:?}", err);
    }

    Ok(())
}

fn local_date_to_utc_ts(d: chrono::NaiveDate) -> i64 {
    let naive = d.and_hms_opt(0, 0, 0).unwrap();
    match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => dt.with_timezone(&Utc).timestamp(),
        chrono::LocalResult::Ambiguous(dt, _) => dt.with_timezone(&Utc).timestamp(),
        // A midnight that doesn't exist locally (rare DST edge case): fall
        // back to treating it as UTC directly rather than failing the
        // whole reload.
        chrono::LocalResult::None => Utc.from_utc_datetime(&naive).timestamp(),
    }
}

fn reload_calendars(app: &mut CalApp, db: &MailDb) {
    if let Ok(calendars) = db.get_calendars(&app.current_account) {
        app.set_calendars(calendars);
    }
}

/// Load the coming year's events (one row per series) for the invitations
/// list and the header's unanswered count — independent of the displayed
/// range.
fn reload_invitations(app: &mut CalApp, db: &MailDb) {
    let now = Utc::now().timestamp();
    let rows = db
        .get_calendar_events_in_range(Some(&app.current_account), now - 3600, now + 365 * 86400)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.dtend_utc > now || r.rrule.is_some())
        .collect();
    app.set_invitations(rows);
}

fn reload_events(app: &mut CalApp, db: &MailDb) {
    // `displayed_range` (not `visible_range`) so Month view's
    // leading/trailing overflow days from adjacent months also get their
    // events loaded, not just the exact calendar month.
    let (start, end) = app.displayed_range();
    let start_ts = local_date_to_utc_ts(start);
    let end_ts = local_date_to_utc_ts(end);
    match db.get_calendar_events_in_range(Some(&app.current_account), start_ts, end_ts) {
        // `get_calendar_events_in_range` returns each recurring event's
        // raw master row unconditionally now (its own DTSTART/DTEND may
        // fall well outside this window) — expand to the occurrences that
        // actually land in `[start_ts, end_ts)` before displaying them.
        Ok(events) => app.set_events_expanding_recurrence(events, start_ts, end_ts),
        Err(e) => app.set_status(format!("Failed to load events: {}", e)),
    }
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut CalApp,
    db: &MailDb,
    ipc_client: &ipc::IpcClient,
) -> Result<()> {
    let mut last_calendar_reload = Instant::now();

    loop {
        terminal.draw(|frame| app.render(frame))?;

        // Drain IPC events: connection lifecycle + calendar sync/mutation
        // progress from jamaild for our account.
        while let Ok(client_ev) = ipc_client.event_rx.try_recv() {
            match client_ev {
                ipc::ClientEvent::Connected => {
                    app.ipc_connected = true;
                    // jamaild may already have synced calendars/events
                    // into the cache before this connection came up (its
                    // own startup, or a reconnect after jamaild
                    // restarted) — reload from the local cache right away
                    // instead of waiting for the next periodic reload or
                    // a pushed sync event, and kick off a fresh sync too.
                    reload_calendars(app, db);
                    reload_events(app, db);
                    ipc_client.send(ipc::Request::SyncCalendars {
                        account: app.current_account.clone(),
                    });
                }
                ipc::ClientEvent::Disconnected(reason) => {
                    app.ipc_connected = false;
                    app.set_status(format!("jamaild unreachable, retrying ({})", reason));
                }
                ipc::ClientEvent::Fatal(reason) => {
                    app.ipc_connected = false;
                    app.set_status(format!("jamaild: {}", reason));
                }
                ipc::ClientEvent::Event(ev) => handle_calendar_event(app, db, ev),
            }
        }

        if last_calendar_reload.elapsed() >= CALENDAR_RELOAD_INTERVAL {
            reload_calendars(app, db);
            reload_events(app, db);
            reload_invitations(app, db);
            last_calendar_reload = Instant::now();
        }

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match app.mode {
                CalMode::Browse => {
                    if !handle_browse_key(app, db, ipc_client, key.code, key.modifiers) {
                        return Ok(());
                    }
                }
                CalMode::Create | CalMode::Edit => {
                    handle_form_key(app, db, ipc_client, key.code, key.modifiers);
                }
                CalMode::ConfirmDelete => {
                    handle_delete_confirm_key(app, db, ipc_client, key.code);
                }
                CalMode::Rsvp => {
                    handle_rsvp_key(app, db, ipc_client, key.code);
                }
                CalMode::Calendars => handle_calendar_panel_key(app, key.code, key.modifiers),
                CalMode::Attendees => {
                    handle_attendee_editor_key(app, db, ipc_client, key.code, key.modifiers);
                }
                CalMode::Invitations => handle_invitations_key(app, db, ipc_client, key.code),
            }
            persist_calendar_prefs(app);
        }
    }
}

fn handle_calendar_event(app: &mut CalApp, db: &MailDb, ev: ipc::Event) {
    match ev {
        ipc::Event::CalendarSyncing { account } if account == app.current_account => {
            app.set_status("Syncing calendars…");
        }
        ipc::Event::CalendarSynced {
            account,
            calendar_url,
            count,
        } if account == app.current_account => {
            reload_calendars(app, db);
            reload_events(app, db);
            if count > 0 {
                app.set_status(format!("Synced {} ({} changed)", calendar_url, count));
            } else {
                app.clear_status();
            }
        }
        ipc::Event::CalendarError { account, message } if account == app.current_account => {
            app.set_status(format!("Calendar sync error: {}", message));
        }
        ipc::Event::CalendarMutationApplied { account, .. } if account == app.current_account => {
            reload_events(app, db);
            app.set_status("Change synced.");
        }
        ipc::Event::CalendarMutationError {
            account, message, ..
        } if account == app.current_account => {
            app.set_status(format!("Sync failed: {}", message));
        }
        ipc::Event::CalendarMutationConflict {
            account, message, ..
        } if account == app.current_account => {
            reload_events(app, db);
            app.set_status(format!("Conflict: {}", message));
        }
        _ => {}
    }
}

/// Returns `false` to request exiting the main loop.
fn handle_browse_key(
    app: &mut CalApp,
    db: &MailDb,
    ipc_client: &ipc::IpcClient,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> bool {
    app.clear_status();
    match code {
        KeyCode::Char('q') => return false,
        // Esc walks back through the views you stepped through, restoring
        // the period and cursor you left behind (CalApp::go_back) — so
        // Enter into a day and Esc puts you back on the month you were
        // reading. It is never a quit key; `q` is the only way out.
        KeyCode::Esc => {
            if app.go_back() {
                reload_events(app, db);
            } else {
                app.set_status("Nothing to go back to — press q to quit.");
            }
        }
        // Move the focused day/cell by one step; in columnar views
        // (Day/3-Day/Week) Up/Down instead moves the selected *event*
        // within the focused day (see CalApp::move_focus_vertical docs).
        // Crossing the edge of what's currently displayed shifts the
        // period automatically, so these also double as "go to the next
        // day even if that's next week".
        KeyCode::Char('h') | KeyCode::Left => {
            app.move_focus_horizontal(-1);
            reload_events(app, db);
        }
        KeyCode::Char('l') | KeyCode::Right => {
            app.move_focus_horizontal(1);
            reload_events(app, db);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.move_focus_vertical(-1);
            reload_events(app, db);
        }
        KeyCode::Char('j') | KeyCode::Down => {
            app.move_focus_vertical(1);
            reload_events(app, db);
        }
        // Jump the whole period at once (a full week/month/year), as
        // opposed to h/j/k/l's one-cell-at-a-time movement.
        KeyCode::Char('[') | KeyCode::PageUp => {
            app.jump_period_backward();
            reload_events(app, db);
        }
        KeyCode::Char(']') | KeyCode::PageDown => {
            app.jump_period_forward();
            reload_events(app, db);
        }
        // Zoom in one level of detail at the focused cell: Year -> Month,
        // Month/Week/3-Day -> Day, Day -> the event under the cursor.
        KeyCode::Enter => {
            app.zoom_in();
            reload_events(app, db);
        }
        // Tab steps one level *narrower* (Year -> Month -> Week -> 3-Day ->
        // Day -> Event) and Shift+Tab one level wider; neither wraps around.
        // Most terminals report Shift+Tab as a bare `BackTab` with no SHIFT
        // modifier bit, so `BackTab` alone has to mean "widen" — checking
        // only the modifier would make Shift+Tab narrow like plain Tab.
        KeyCode::BackTab => {
            app.widen_view();
            reload_events(app, db);
        }
        KeyCode::Tab => {
            if modifiers.contains(KeyModifiers::SHIFT) {
                app.widen_view();
            } else {
                app.narrow_view();
            }
            reload_events(app, db);
        }
        KeyCode::Char('t') => {
            app.goto_today();
            reload_events(app, db);
        }
        // The calendar panel: `C`, or `v` for terminals whose keyboard
        // protocol reports Shift+c as a lowercase `c` with the SHIFT
        // modifier rather than as `C`.
        KeyCode::Char('C') | KeyCode::Char('v') => app.begin_calendar_panel(),
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::SHIFT) => app.begin_calendar_panel(),
        KeyCode::Char('n') | KeyCode::Char('c') => {
            if app.calendars.is_empty() {
                app.set_status("No calendars discovered yet — try 's' to sync.");
            } else {
                app.begin_create();
            }
        }
        KeyCode::Char('e') => {
            if let Err(e) = app.begin_edit() {
                app.set_status(e);
            }
        }
        KeyCode::Char('d') => {
            if let Err(e) = app.begin_delete_confirm() {
                app.set_status(e);
            }
        }
        KeyCode::Char('r') => {
            if let Err(e) = app.begin_rsvp() {
                app.set_status(e);
            }
        }
        KeyCode::Char('a') => {
            if let Err(e) = app.begin_attendees_for_selected() {
                app.set_status(e);
            }
        }
        KeyCode::Char('i') => app.begin_invitations(),
        KeyCode::Char(digit @ '1'..='9') => {
            let idx = digit as usize - '1' as usize;
            if let Some(cal) = app.calendars.get(idx).cloned() {
                app.toggle_calendar_visible(&cal.url);
            }
        }
        KeyCode::Char('s') => {
            ipc_client.send(ipc::Request::SyncCalendars {
                account: app.current_account.clone(),
            });
            app.set_status("Requested sync…");
        }
        // Say so when a key does nothing: it also shows exactly what the
        // terminal delivered, which is what matters when a Shift+letter
        // binding seems dead.
        KeyCode::Char(c) => app.set_status(format!(
            "No action for {:?} ({:?}) — see the key hints below",
            c, modifiers
        )),
        _ => {}
    }
    true
}

fn handle_form_key(
    app: &mut CalApp,
    db: &MailDb,
    ipc_client: &ipc::IpcClient,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    // Status-cycling uses Ctrl+X (rather than plain 'x') specifically so
    // every free-text field (Summary/Location/Description/Repeats) can
    // still contain a literal 'x' — see handle_form_key's Char(c) arm
    // below for normal text input.
    if code == KeyCode::Char('x') && modifiers.contains(KeyModifiers::CONTROL) {
        if let Some(draft) = app.draft.as_mut() {
            draft.cycle_status();
        }
        return;
    }
    match code {
        KeyCode::Esc => app.cancel_form(),
        // Down/Tab move to the next field, Up/Shift+Tab to the previous —
        // the arrows are what most people reach for in a form, and nothing
        // else in this modal needs them (the fields are single-line, so
        // there is no vertical cursor movement to compete with).
        KeyCode::Tab | KeyCode::Down => {
            if let Some(draft) = app.draft.as_mut() {
                draft.next_field();
            }
        }
        KeyCode::BackTab | KeyCode::Up => {
            if let Some(draft) = app.draft.as_mut() {
                draft.prev_field();
            }
        }
        // The calendar field picks where a new event lives; the organizer
        // identity follows the calendar.
        KeyCode::Left | KeyCode::Right
            if app
                .draft
                .as_ref()
                .is_some_and(|d| d.field == CalField::Calendar) =>
        {
            let step = if code == KeyCode::Right { 1 } else { -1 };
            let calendars = app.calendars.clone();
            if let Some(draft) = app.draft.as_mut() {
                draft.cycle_calendar(&calendars, step);
            }
            let organizer = app
                .draft
                .as_ref()
                .and_then(|d| app.calendar_identity(&d.calendar_url));
            if let Some(draft) = app.draft.as_mut() {
                draft.organizer = organizer;
            }
        }
        KeyCode::Enter
            if app
                .draft
                .as_ref()
                .is_some_and(|d| d.field == CalField::Attendees) =>
        {
            app.begin_attendees_for_draft();
        }
        KeyCode::Char(' ')
            if app
                .draft
                .as_ref()
                .map(|d| d.field == CalField::AllDay)
                .unwrap_or(false) =>
        {
            if let Some(draft) = app.draft.as_mut() {
                draft.toggle_all_day();
            }
        }
        KeyCode::Char(c) => {
            if let Some(draft) = app.draft.as_mut() {
                draft.input_char(c);
            }
        }
        KeyCode::Backspace => {
            if let Some(draft) = app.draft.as_mut() {
                draft.backspace();
            }
        }
        KeyCode::Enter => submit_form(app, db, ipc_client),
        _ => {}
    }
}

fn submit_form(app: &mut CalApp, db: &MailDb, ipc_client: &ipc::IpcClient) {
    let Some(draft) = app.draft.clone() else {
        return;
    };
    let account = app.current_account.clone();

    if let Some(local_id) = draft.editing_local_id {
        match draft.to_event_edits() {
            Ok(edits) => match db.apply_local_calendar_edit(local_id, &edits) {
                Ok(()) => {
                    ipc_client.send(ipc::Request::EnqueueCalendarMutation {
                        account,
                        mutation: CalMutation::Update { local_id, edits },
                    });
                    reload_events(app, db);
                    app.cancel_form();
                    app.set_status("Saved, syncing…");
                }
                Err(e) => app.set_status(format!("Could not save edit: {}", e)),
            },
            Err(e) => app.set_status(e),
        }
    } else {
        match draft.to_new_vevent() {
            Ok(event) => {
                match db.insert_local_calendar_event(&account, &draft.calendar_url, &event) {
                    Ok(local_id) => {
                        ipc_client.send(ipc::Request::EnqueueCalendarMutation {
                            account,
                            mutation: CalMutation::Create { local_id },
                        });
                        reload_events(app, db);
                        app.cancel_form();
                        app.set_status("Created, syncing…");
                    }
                    Err(e) => app.set_status(format!("Could not save event: {}", e)),
                }
            }
            Err(e) => app.set_status(e),
        }
    }
}

/// Keys in the calendar panel (`C`): move, show/hide, recolour, close.
fn handle_calendar_panel_key(app: &mut CalApp, code: KeyCode, modifiers: KeyModifiers) {
    app.clear_status();
    match code {
        KeyCode::Char('j') | KeyCode::Down => app.calendar_panel_move(1),
        KeyCode::Char('k') | KeyCode::Up => app.calendar_panel_move(-1),
        KeyCode::Char(' ') | KeyCode::Enter => {
            if let Some(url) = app.panel_calendar_url() {
                app.toggle_calendar_visible(&url);
            }
        }
        KeyCode::Char('c') | KeyCode::Char('l') | KeyCode::Right => {
            if let Some(url) = app.panel_calendar_url() {
                app.cycle_calendar_color(&url, 1);
            }
        }
        KeyCode::Char('h') | KeyCode::Left => {
            if let Some(url) = app.panel_calendar_url() {
                app.cycle_calendar_color(&url, -1);
            }
        }
        KeyCode::Char('x') | KeyCode::Backspace | KeyCode::Delete => {
            if let Some(url) = app.panel_calendar_url() {
                app.clear_calendar_color(&url);
            }
        }
        // `d`/`D` (either case: some terminals report Shift+d as a lowercase
        // `d` with the SHIFT modifier) shows or hides declined events.
        KeyCode::Char('d') | KeyCode::Char('D') => app.toggle_show_declined(),
        KeyCode::Char(digit @ '1'..='9') => {
            let idx = digit as usize - '1' as usize;
            if let Some(cal) = app.calendars.get(idx).cloned() {
                app.toggle_calendar_visible(&cal.url);
            }
        }
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('C') | KeyCode::Char('v') => {
            app.close_calendar_panel()
        }
        KeyCode::Char(c) => app.set_status(format!(
            "No panel action for {:?} ({:?}); keys: j/k space c ← → x d Esc",
            c, modifiers
        )),
        _ => {}
    }
}

/// Write `jacal.calendars` back to the config file when a toggle or a
/// colour change flagged it (see `CalApp::take_prefs_dirty`). Only that
/// list is rewritten; the rest of the file stays byte for byte.
fn persist_calendar_prefs(app: &mut CalApp) {
    if !app.take_prefs_dirty() {
        return;
    }
    let result = config::config_path().and_then(|path| {
        config::save_jacal_settings(&path, &app.calendar_prefs, app.show_declined)
    });
    if let Err(e) = result {
        app.set_status(format!("Could not save calendar settings: {:#}", e));
    }
}

/// Keys in the participants modal: type an address and Enter to add it,
/// Up/Down to pick a guest, Delete (or Ctrl+D) to remove, Esc to finish —
/// which saves the new guest list when the modal was opened on an
/// existing event and something changed.
fn handle_attendee_editor_key(
    app: &mut CalApp,
    db: &MailDb,
    ipc_client: &ipc::IpcClient,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    match code {
        KeyCode::Esc => {
            if let Some(commit) = app.finish_attendee_editor() {
                let account = app.current_account.clone();
                match db.apply_local_calendar_edit(commit.id, &commit.edits) {
                    Ok(()) => {
                        ipc_client.send(ipc::Request::EnqueueCalendarMutation {
                            account,
                            mutation: CalMutation::Update {
                                local_id: commit.id,
                                edits: commit.edits,
                            },
                        });
                        reload_events(app, db);
                        app.set_status(format!(
                            "Guest list of \"{}\" saved, syncing…",
                            commit.summary
                        ));
                    }
                    Err(e) => app.set_status(format!("Could not save guests: {}", e)),
                }
            }
        }
        KeyCode::Enter => {
            if let Err(e) = app.attendee_editor_add() {
                app.set_status(e);
            }
        }
        KeyCode::Up => app.attendee_editor_move(-1),
        KeyCode::Down => app.attendee_editor_move(1),
        KeyCode::Delete => app.attendee_editor_remove(),
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.attendee_editor_remove()
        }
        KeyCode::Backspace => app.attendee_editor_backspace(),
        KeyCode::Char(c) => app.attendee_editor_input(c),
        _ => {}
    }
}

/// Every address the account can answer an invitation as: its `email`,
/// its `caldav.login` and every `senders` entry (display names stripped),
/// lower-cased and de-duplicated. Matched against `ATTENDEE` lines by
/// `CalApp::own_attendee`.
fn own_identities(account: &config::JamailAccount) -> Vec<String> {
    let mut raw: Vec<String> = vec![account.email.clone()];
    if let Some(c) = &account.caldav {
        raw.push(c.login.clone());
    }
    if let Some(senders) = &account.senders {
        raw.extend(senders.iter().cloned());
    }
    let mut out: Vec<String> = Vec::new();
    for entry in raw {
        let addr = jamail::jadav::config::mailbox_address(&entry);
        if addr.contains('@') && !out.contains(&addr) {
            out.push(addr);
        }
    }
    out
}

/// Answer the invitation the RSVP prompt was opened for: rewrite our
/// `ATTENDEE` line locally (no `SEQUENCE` bump) and queue the same
/// `Update` mutation an edit uses; the CalDAV server turns the stored
/// `PARTSTAT` change into the iTIP `REPLY` mail.
fn handle_rsvp_key(app: &mut CalApp, db: &MailDb, ipc_client: &ipc::IpcClient, code: KeyCode) {
    let (partstat, verb) = match code {
        KeyCode::Char('a') | KeyCode::Enter => ("ACCEPTED", "Accepted"),
        KeyCode::Char('t') => ("TENTATIVE", "Tentatively accepted"),
        KeyCode::Char('d') => ("DECLINED", "Declined"),
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
            app.cancel_rsvp();
            return;
        }
        _ => return,
    };
    let Some(target) = app.take_pending_rsvp() else {
        return;
    };
    apply_rsvp(app, db, ipc_client, &target, partstat, verb);
}

/// Answer `target`'s invitation with `partstat`: rewrite our ATTENDEE line
/// locally and queue the Update the server turns into the iTIP REPLY.
fn apply_rsvp(
    app: &mut CalApp,
    db: &MailDb,
    ipc_client: &ipc::IpcClient,
    target: &jamail::calapp::PendingRsvp,
    partstat: &str,
    verb: &str,
) {
    let account = app.current_account.clone();
    let edits = EventEdits {
        rsvp: Some(Rsvp {
            attendee: target.attendee.clone(),
            partstat: partstat.to_string(),
        }),
        ..Default::default()
    };
    match db.apply_local_calendar_edit(target.id, &edits) {
        Ok(()) => {
            ipc_client.send(ipc::Request::EnqueueCalendarMutation {
                account,
                mutation: CalMutation::Update {
                    local_id: target.id,
                    edits,
                },
            });
            reload_events(app, db);
            reload_invitations(app, db);
            app.set_status(format!("{} \"{}\", syncing…", verb, target.summary));
        }
        Err(e) => app.set_status(format!("Could not respond: {}", e)),
    }
}

/// Keys in the invitations panel: `a`/`t`/`d` answer the highlighted
/// invitation (the list re-sorts, so the cursor lands on the next new
/// one), Enter opens it, j/k move, Esc closes.
fn handle_invitations_key(
    app: &mut CalApp,
    db: &MailDb,
    ipc_client: &ipc::IpcClient,
    code: KeyCode,
) {
    let answer = match code {
        KeyCode::Char('a') => Some(("ACCEPTED", "Accepted")),
        KeyCode::Char('t') => Some(("TENTATIVE", "Tentatively accepted")),
        KeyCode::Char('d') => Some(("DECLINED", "Declined")),
        _ => None,
    };
    if let Some((partstat, verb)) = answer {
        match app.invitation_rsvp_target() {
            Ok(target) => apply_rsvp(app, db, ipc_client, &target, partstat, verb),
            Err(e) => app.set_status(e),
        }
        return;
    }
    match code {
        KeyCode::Char('j') | KeyCode::Down => app.invitations_move(1),
        KeyCode::Char('k') | KeyCode::Up => app.invitations_move(-1),
        KeyCode::Enter => {
            if app.jump_to_selected_invitation() {
                reload_events(app, db);
            }
        }
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('i') => app.close_invitations(),
        _ => {}
    }
}

fn handle_delete_confirm_key(
    app: &mut CalApp,
    db: &MailDb,
    ipc_client: &ipc::IpcClient,
    code: KeyCode,
) {
    match code {
        KeyCode::Char('y') | KeyCode::Enter => {
            // Act on the event the prompt was opened for, not on whatever
            // the cursor happens to be on now: a sync landing while the
            // prompt was up can have moved it (see calapp::PendingDelete).
            let Some(target) = app.take_pending_delete() else {
                return;
            };
            let account = app.current_account.clone();
            match db.mark_calendar_event_pending_delete(target.id) {
                // The row was only ever local and is already gone; queuing
                // a Delete for its id would be worse than useless, since
                // SQLite can hand that id to the next event created.
                Ok(false) => {
                    reload_events(app, db);
                    app.set_status("Deleted.");
                }
                Ok(true) => {
                    ipc_client.send(ipc::Request::EnqueueCalendarMutation {
                        account,
                        mutation: CalMutation::Delete {
                            local_id: target.id,
                        },
                    });
                    reload_events(app, db);
                    app.set_status("Deleting, syncing…");
                }
                Err(e) => app.set_status(format!("Could not delete: {}", e)),
            }
        }
        KeyCode::Char('n') | KeyCode::Esc => app.cancel_delete_confirm(),
        _ => {}
    }
}
