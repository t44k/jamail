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
//! Calendar alarm notifications are delivered by *this* process while
//! it's running (see `jamail::calnotify` module docs for why that's a
//! different ownership model than `jamail::notify`'s daemon-owned mail
//! notifications, and what that implies: no `jacal` running means no
//! alarm popups, the same way no terminal-based calendar client's alarms
//! fire while it's closed unless a separate always-on piece exists for
//! that, which this task's scope didn't include).

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Local, TimeZone, Utc};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use jamail::calapp::{CalApp, CalField, CalMode};
use jamail::calendar::{EventTime, VEvent, expand_occurrences};
use jamail::calnotify::{AlarmScheduler, DesktopAlarmSink};
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
/// How often to re-check for due alarms. Alarms only need roughly
/// minute-level precision (see `calnotify::DEFAULT_GRACE`), so this can be
/// coarser than the calendar reload interval.
const ALARM_CHECK_INTERVAL: Duration = Duration::from_secs(20);
/// How far ahead (and slightly behind, to catch an alarm whose trigger
/// just passed) to load events for alarm scheduling — independent of
/// whatever date range is currently displayed.
const ALARM_LOOKAHEAD: ChronoDuration = ChronoDuration::hours(48);

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
    let default_lead_minutes: Vec<i64> = caldav_accounts[0]
        .1
        .caldav
        .as_ref()
        .and_then(|c| c.default_alarm_minutes_before.clone())
        .unwrap_or_default();

    let db = MailDb::open().context("Failed to open mail database")?;

    let mut app = CalApp::new(current_account.clone(), week_start);
    reload_calendars(&mut app, &db);
    reload_events(&mut app, &db);

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

    let mut scheduler = AlarmScheduler::new();
    let alarm_sink = DesktopAlarmSink;

    let result = run_app(
        &mut terminal,
        &mut app,
        &db,
        &ipc_client,
        &mut scheduler,
        &alarm_sink,
        &default_lead_minutes,
    );

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

/// Load events across a wide window (independent of whatever range is
/// currently displayed) for alarm scheduling, reconstructing full
/// `VEvent`s (with `VALARM`s) from each row's stored raw ICS and expanding
/// recurring events to every occurrence within the window, each with its
/// `dtstart`/`dtend` shifted to that occurrence's own instants (so a
/// relative `VALARM`, or the default-lead-minutes fallback, fires once per
/// occurrence rather than only ever for the series' master).
fn load_events_for_alarms(db: &MailDb, account: &str) -> Vec<VEvent> {
    let now = Utc::now();
    let start_ts = (now - ChronoDuration::hours(1)).timestamp();
    let end_ts = (now + ALARM_LOOKAHEAD).timestamp();
    let rows = db
        .get_calendar_events_in_range(Some(account), start_ts, end_ts)
        .unwrap_or_default();
    let (Some(window_start), Some(window_end)) = (
        DateTime::<Utc>::from_timestamp(start_ts, 0),
        DateTime::<Utc>::from_timestamp(end_ts, 0),
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for row in &rows {
        let Ok(vevent) = row.to_vevent() else {
            continue;
        };
        for (occ_start, occ_end) in expand_occurrences(&vevent, window_start, window_end) {
            let mut occurrence = vevent.clone();
            occurrence.dtstart = EventTime::utc(occ_start);
            occurrence.dtend = EventTime::utc(occ_end);
            out.push(occurrence);
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut CalApp,
    db: &MailDb,
    ipc_client: &ipc::IpcClient,
    scheduler: &mut AlarmScheduler,
    alarm_sink: &DesktopAlarmSink,
    default_lead_minutes: &[i64],
) -> Result<()> {
    let mut last_calendar_reload = Instant::now();
    let mut last_alarm_check = Instant::now() - ALARM_CHECK_INTERVAL; // check once immediately
    let mut alarm_events = load_events_for_alarms(db, &app.current_account);

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
            last_calendar_reload = Instant::now();
        }

        if last_alarm_check.elapsed() >= ALARM_CHECK_INTERVAL {
            alarm_events = load_events_for_alarms(db, &app.current_account);
            last_alarm_check = Instant::now();
        }
        let fired = scheduler.tick(&alarm_events, default_lead_minutes, Utc::now(), alarm_sink);
        for (fire, outcome) in fired {
            match outcome {
                Ok(()) => {}
                Err(e) => app.set_status(format!(
                    "Alarm for \"{}\" failed to deliver: {}",
                    fire.summary, e
                )),
            }
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
            }
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
        KeyCode::Char('q') | KeyCode::Esc => return false,
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
        // Month/Week/3-Day -> Day. No-op already in Day view.
        KeyCode::Enter => {
            app.zoom_in();
            reload_events(app, db);
        }
        KeyCode::Tab if modifiers.contains(KeyModifiers::SHIFT) => {
            app.cycle_view_backward();
            reload_events(app, db);
        }
        KeyCode::Tab | KeyCode::BackTab => {
            if modifiers.contains(KeyModifiers::SHIFT) {
                app.cycle_view_backward();
            } else {
                app.cycle_view_forward();
            }
            reload_events(app, db);
        }
        KeyCode::Char('t') => {
            app.goto_today();
            reload_events(app, db);
        }
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
        KeyCode::Tab => {
            if let Some(draft) = app.draft.as_mut() {
                draft.next_field();
            }
        }
        KeyCode::BackTab => {
            if let Some(draft) = app.draft.as_mut() {
                draft.prev_field();
            }
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

fn handle_delete_confirm_key(
    app: &mut CalApp,
    db: &MailDb,
    ipc_client: &ipc::IpcClient,
    code: KeyCode,
) {
    match code {
        KeyCode::Char('y') | KeyCode::Enter => {
            if let Some(id) = app.selected_event().map(|e| e.id) {
                let account = app.current_account.clone();
                if let Err(e) = db.mark_calendar_event_pending_delete(id) {
                    app.set_status(format!("Could not delete: {}", e));
                } else {
                    ipc_client.send(ipc::Request::EnqueueCalendarMutation {
                        account,
                        mutation: CalMutation::Delete { local_id: id },
                    });
                    reload_events(app, db);
                    app.set_status("Deleting, syncing…");
                }
            }
            app.mode = CalMode::Browse;
        }
        KeyCode::Char('n') | KeyCode::Esc => {
            app.mode = CalMode::Browse;
        }
        _ => {}
    }
}
