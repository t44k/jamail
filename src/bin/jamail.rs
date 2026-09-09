//! `jamail` — the foreground terminal UI client.
//!
//! Reads the shared local SQLite cache directly (same as before the
//! daemon/client split) and talks to the `jamaild` background daemon over
//! the IPC protocol in `jamail::ipc` for anything that needs a live IMAP
//! session: which folder to prioritize for IDLE, mark-seen, and Sent/Draft
//! folder uploads. All accounts sync continuously in `jamaild` regardless
//! of what this client is looking at — see `jamail::daemon` module docs.
//!
//! If no `jamaild` is reachable at startup, one is auto-spawned (see
//! `jamail::daemon::try_autostart`); either way this binary never blocks
//! waiting for it — the UI renders immediately from the local cache and the
//! IPC connection comes up in the background, with its own status-bar
//! indicator while disconnected/reconnecting.

use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        KeyboardEnhancementFlags, MouseButton, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        supports_keyboard_enhancement,
    },
};
use jamail::app::{App, ComposeField, DetailMode, ViewMode};
use jamail::{app, config, daemon, db, ipc, smtp};
use ratatui::prelude::*;
use ratatui_image::picker::Picker;
use std::io::{self, IsTerminal};
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    if !io::stdout().is_terminal() {
        anyhow::bail!("jamail requires an interactive terminal (TTY) to run");
    }

    // Load config
    let config = config::JamailConfig::load().context("Failed to load jamail config")?;
    let (default_name, _default_account) = config.default_account()?;
    let default_name = default_name.to_string();
    let daemon_socket_override = config.daemon.as_ref().and_then(|d| d.socket_path.clone());

    let accounts: Vec<(String, config::JamailAccount)> = config.accounts.into_iter().collect();

    // Open database (creates on first run)
    let db = db::MailDb::open().context("Failed to open mail database")?;

    let current_folder = "INBOX".to_string();

    // Load cached emails immediately
    let emails = db
        .get_email_list(&default_name, &current_folder)
        .context("Failed to load emails from database")?;

    // Load cached folders
    let cached_folders = db.get_folders(&default_name).unwrap_or_default();

    // Connect to jamaild over IPC (auto-spawning one if unreachable). This
    // never blocks: the IpcClient's own reconnect loop brings the
    // connection up in the background regardless of whether jamaild was
    // already running, was just spawned, or stays unreachable for a while.
    let socket_path = ipc::resolve_socket_path(daemon_socket_override.as_deref());
    daemon::try_autostart(&socket_path);
    let ipc_client = ipc::IpcClient::spawn(socket_path);
    ipc_client.send(ipc::Request::SetCurrentFolder {
        account: default_name.clone(),
        folder: current_folder.clone(),
    });

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    // Enable keyboard enhancement if the terminal supports it (kitty protocol).
    // This makes Ctrl+Enter distinguishable from plain Enter.
    let keyboard_enhanced = supports_keyboard_enhancement().unwrap_or(false);
    if keyboard_enhanced {
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Detect terminal graphics protocol for image previews
    let picker = Picker::from_query_stdio().ok();

    // Create the persistent send queue (background worker thread).
    let send_queue = smtp::SendQueue::new();

    // Run app
    let mut app = App::new(
        emails,
        picker,
        accounts.clone(),
        default_name.clone(),
        current_folder,
    );
    // Share the queue's counters with App so the status bar can read them.
    app.send_counters = std::sync::Arc::clone(&send_queue.counters);

    // Populate cached folders
    if !cached_folders.is_empty() {
        app.account_folders
            .insert(default_name.clone(), cached_folders);
    }

    let result = run_app(
        &mut terminal,
        &mut app,
        &db,
        &ipc_client,
        &accounts,
        send_queue,
    );

    // Restore terminal
    if keyboard_enhanced {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = result {
        eprintln!("Error: {:?}", err);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    db: &db::MailDb,
    ipc_client: &ipc::IpcClient,
    accounts: &[(String, config::JamailAccount)],
    send_queue: smtp::SendQueue,
) -> Result<()> {
    // Track time between loop iterations to detect sleep/wake
    let mut last_loop_time = Instant::now();

    loop {
        let cf = terminal.draw(|frame| {
            app.render(frame);
            app.render_selection(frame);
        })?;

        // Drain IPC events (non-blocking) — sync progress from jamaild for
        // every account it manages, plus connection-lifecycle notices.
        while let Ok(client_ev) = ipc_client.event_rx.try_recv() {
            match client_ev {
                ipc::ClientEvent::Connected => {
                    if !app.status_sticky && app.status_msg.starts_with("jamaild") {
                        app.status_msg.clear();
                    }
                }
                ipc::ClientEvent::Disconnected(reason) => {
                    app.spinner_active = false;
                    if !app.status_sticky {
                        app.status_msg = format!("jamaild unreachable, retrying ({})", reason);
                    }
                }
                ipc::ClientEvent::Fatal(reason) => {
                    app.status_msg = format!("jamaild: {}", reason);
                    app.status_sticky = true;
                }
                ipc::ClientEvent::Event(ev) => handle_sync_event(app, db, ev),
            }
        }

        // Drain completed send-queue results.
        while let Ok(result) = send_queue.result_rx.try_recv() {
            let account = result.account.clone();
            if let Some(upload) = app.handle_send_result(db, result) {
                send_upload_request(ipc_client, &account, upload);
            }
        }

        // Poll for events with 200ms timeout
        if !event::poll(Duration::from_millis(200))? {
            // Check for sleep/wake: if the loop took much longer than 200ms
            let loop_elapsed = last_loop_time.elapsed();
            last_loop_time = Instant::now();
            if loop_elapsed > Duration::from_secs(70) {
                ipc_client.send(ipc::Request::ForceReconnect {
                    account: app.current_account.clone(),
                });
            }
            continue;
        }
        last_loop_time = Instant::now();

        match event::read()? {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                app.clear_selection();
                if app.status_sticky {
                    app.status_msg.clear();
                    app.status_sticky = false;
                }

                // Help overlay: ? toggles in all modes except Compose
                if app.show_help {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('?') => app.show_help = false,
                        KeyCode::Down => {
                            app.help_scroll = app.help_scroll.saturating_add(1);
                        }
                        KeyCode::Up => {
                            app.help_scroll = app.help_scroll.saturating_sub(1);
                        }
                        _ => app.show_help = false,
                    }
                    continue;
                }
                if key.code == KeyCode::Char('?') && app.view != ViewMode::Compose {
                    app.show_help = true;
                    app.help_scroll = 0;
                    continue;
                }

                match &app.view {
                    ViewMode::FolderSelect => match key.code {
                        KeyCode::Char('q') => {
                            app.should_quit = true;
                            return Ok(());
                        }
                        KeyCode::Esc | KeyCode::Left => {
                            app.view = ViewMode::List;
                        }
                        KeyCode::Up => app.folder_tree_up(),
                        KeyCode::Down => app.folder_tree_down(),
                        KeyCode::Tab => app.folder_tree_toggle_expand(),
                        KeyCode::Enter | KeyCode::Right => {
                            if let Some((acct_name, folder_name)) = app.folder_tree_select() {
                                if acct_name == "*virtual*" {
                                    // Virtual folder (Drafts/Sent)
                                    app.load_virtual_folder(db, &folder_name);
                                    app.view = ViewMode::List;
                                } else if acct_name == "*global*" {
                                    // Global inbox mode — read-only cross-account view
                                    app.is_global_inbox = true;
                                    app.virtual_folder = None;
                                    app.local_messages.clear();
                                    app.refresh_emails(db);
                                    app.selected = 0;
                                    app.list_scroll_offset = 0;
                                    app.view = ViewMode::List;
                                } else {
                                    // Real account+folder. jamaild syncs every
                                    // configured account continuously regardless
                                    // of this selection — switching here just
                                    // loads the cache for the new account/folder
                                    // and tells jamaild which one to prioritize
                                    // for IMAP IDLE (no thread spawn/teardown
                                    // needed client-side any more).
                                    app.is_global_inbox = false;
                                    app.virtual_folder = None;
                                    app.local_messages.clear();
                                    app.current_account = acct_name.clone();
                                    app.current_folder = folder_name.clone();
                                    app.refresh_emails(db);
                                    app.selected = 0;
                                    app.list_scroll_offset = 0;

                                    ipc_client.send(ipc::Request::SetCurrentFolder {
                                        account: acct_name,
                                        folder: folder_name,
                                    });

                                    app.view = ViewMode::List;
                                }
                            }
                        }
                        _ => {}
                    },
                    ViewMode::List => {
                        let page = terminal
                            .size()
                            .map(|s| s.height.saturating_sub(2) as usize)
                            .unwrap_or(20);
                        // Ctrl+D: delete selected draft (only in Drafts folder)
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && key.code == KeyCode::Char('d')
                            && app.virtual_folder.as_deref() == Some("Drafts")
                        {
                            app.delete_selected_draft(db);
                        } else {
                            match key.code {
                                KeyCode::Char('q') => {
                                    app.should_quit = true;
                                    return Ok(());
                                }
                                KeyCode::Down => app.move_down(),
                                KeyCode::Up => app.move_up(),
                                KeyCode::PageDown => app.page_down(page),
                                KeyCode::PageUp => app.page_up(page),
                                KeyCode::Home | KeyCode::Char('g') => app.go_home(),
                                KeyCode::End | KeyCode::Char('G') => app.go_end(),
                                KeyCode::Left => app.enter_folder_select(),
                                KeyCode::Enter | KeyCode::Right => {
                                    if app.virtual_folder.as_deref() == Some("Drafts") {
                                        // Resume editing the draft
                                        app.resume_draft(db, app.selected);
                                    } else {
                                        app.open_detail(db);
                                        enqueue_mark_seen(app, db, ipc_client);
                                    }
                                }
                                KeyCode::Tab | KeyCode::Char('l') => app.toggle_thread_expand(),
                                KeyCode::Char(' ') => app.next_thread(),
                                KeyCode::BackTab => app.prev_thread(),
                                KeyCode::Char('t') => app.toggle_thread_mode(),
                                KeyCode::Char('/') => app.enter_search(),
                                KeyCode::Char('F') => app.enter_folder_select(),
                                KeyCode::Char('n') => app.enter_compose_new(db),
                                _ => {}
                            }
                        } // end Ctrl+D branch
                    }
                    ViewMode::Detail => {
                        let page = terminal
                            .size()
                            .map(|s| s.height.saturating_sub(2))
                            .unwrap_or(20);
                        match key.code {
                            KeyCode::Char('q') => {
                                app.should_quit = true;
                                return Ok(());
                            }
                            KeyCode::Esc | KeyCode::Backspace => {
                                if app.detail_mode != DetailMode::Text {
                                    app.detail_mode = DetailMode::Text;
                                } else {
                                    app.close_detail();
                                }
                            }
                            KeyCode::Down => match app.detail_mode {
                                DetailMode::Text => {
                                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                                        app.next_link();
                                    } else {
                                        app.scroll_detail_down();
                                    }
                                }
                                DetailMode::Attachments => {
                                    app.next_attachment();
                                    app.update_image_preview(db);
                                }
                                DetailMode::Links => app.next_link(),
                            },
                            KeyCode::Up => match app.detail_mode {
                                DetailMode::Text => {
                                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                                        app.prev_link();
                                    } else {
                                        app.scroll_detail_up();
                                    }
                                }
                                DetailMode::Attachments => {
                                    app.prev_attachment();
                                    app.update_image_preview(db);
                                }
                                DetailMode::Links => app.prev_link(),
                            },
                            KeyCode::Left => match app.detail_mode {
                                DetailMode::Text => {
                                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                                        app.prev_in_list(db);
                                        enqueue_mark_seen(app, db, ipc_client);
                                    } else {
                                        app.close_detail();
                                    }
                                }
                                DetailMode::Attachments => {
                                    app.prev_attachment();
                                    app.update_image_preview(db);
                                }
                                DetailMode::Links => app.prev_link(),
                            },
                            KeyCode::Right => match app.detail_mode {
                                DetailMode::Text => {
                                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                                        app.next_in_list(db);
                                        enqueue_mark_seen(app, db, ipc_client);
                                    }
                                    // Plain Right in Text mode: no action
                                }
                                DetailMode::Attachments => {
                                    app.next_attachment();
                                    app.update_image_preview(db);
                                }
                                DetailMode::Links => app.next_link(),
                            },
                            KeyCode::Enter => match app.detail_mode {
                                DetailMode::Links => app.open_selected_link(db),
                                DetailMode::Attachments => app.open_attachment(db),
                                DetailMode::Text => {
                                    if !app.detail_links.is_empty() {
                                        app.open_selected_link(db);
                                    }
                                }
                            },
                            KeyCode::Char('s') => {
                                if app.detail_mode == DetailMode::Attachments {
                                    app.save_attachment(db);
                                }
                            }
                            KeyCode::Char('/') => {
                                app.cycle_detail_mode();
                                app.update_image_preview(db);
                            }
                            KeyCode::Char(' ') => app.scroll_detail_page_down(page),
                            KeyCode::PageDown => {
                                if key.modifiers.contains(KeyModifiers::SHIFT) {
                                    app.next_in_thread(db);
                                    enqueue_mark_seen(app, db, ipc_client);
                                } else {
                                    app.scroll_detail_page_down(page);
                                }
                            }
                            KeyCode::PageUp => {
                                if key.modifiers.contains(KeyModifiers::SHIFT) {
                                    app.prev_in_thread(db);
                                    enqueue_mark_seen(app, db, ipc_client);
                                } else {
                                    app.scroll_detail_page_up(page);
                                }
                            }
                            KeyCode::Home => app.scroll_detail_home(),
                            KeyCode::End => app.scroll_detail_end(),
                            KeyCode::Char('n') => app.enter_compose_new(db),
                            KeyCode::Char('r') => app.enter_reply(db),
                            KeyCode::Char('f') => app.enter_forward(db),
                            KeyCode::Char('h') => app.toggle_raw_headers(),
                            KeyCode::Char('v') => app.open_in_browser(),
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                if let Some(target) = app.ctrl_c_copy_target() {
                                    let target = target.to_string();
                                    copy_to_clipboard(&target);
                                    app.status_msg = format!("Copied {}", target);
                                }
                            }
                            _ => {}
                        }
                    }
                    ViewMode::Search => match key.code {
                        KeyCode::Esc | KeyCode::Left => app.exit_search(),
                        KeyCode::Enter | KeyCode::Right => {
                            if !app.search_results.is_empty() {
                                app.open_detail(db);
                                enqueue_mark_seen(app, db, ipc_client);
                            } else {
                                app.execute_search(db);
                            }
                        }
                        KeyCode::Backspace => app.search_backspace(),
                        KeyCode::Char(c) => app.search_input(c),
                        KeyCode::Down => app.search_move_down(),
                        KeyCode::Up => app.search_move_up(),
                        _ => {}
                    },
                    ViewMode::Compose => {
                        // Global compose keys (checked first)
                        let modifiers = key.modifiers;
                        let is_ctrl = modifiers.contains(KeyModifiers::CONTROL);

                        // Ctrl+X to send (Ctrl+Enter is not reliably detectable on most terminals)
                        if is_ctrl && key.code == KeyCode::Char('x') {
                            handle_compose_send(app, db, accounts, &send_queue);
                            continue;
                        }

                        // Ctrl+A to open file browser
                        if is_ctrl && key.code == KeyCode::Char('a') {
                            app.open_file_browser();
                            continue;
                        }

                        // Ctrl+D to remove last attachment
                        if is_ctrl && key.code == KeyCode::Char('d') {
                            app.compose_remove_last_attachment();
                            continue;
                        }

                        // Ctrl+S to save draft
                        if is_ctrl && key.code == KeyCode::Char('s') {
                            let was_new = app.save_compose_as_draft(db);
                            // Only upload once per draft (on first save) to
                            // avoid piling up duplicate remote copies on
                            // every subsequent edit.
                            if was_new && let Some(upload) = app.enqueue_draft_upload(db) {
                                send_upload_request(ipc_client, &app.current_account, upload);
                            }
                            continue;
                        }

                        match app.compose_field {
                            ComposeField::From => match key.code {
                                KeyCode::Left => app.cycle_sender_backward(),
                                KeyCode::Right => app.cycle_sender_forward(),
                                KeyCode::Tab | KeyCode::Down | KeyCode::Enter => {
                                    app.compose_next_field();
                                }
                                KeyCode::Esc => {
                                    app.cancel_compose();
                                }
                                _ => {}
                            },
                            ComposeField::FileBrowser => match key.code {
                                KeyCode::Up => app.filebrowser_up(),
                                KeyCode::Down => app.filebrowser_down(),
                                KeyCode::Enter => app.filebrowser_enter(),
                                KeyCode::Backspace => app.filebrowser_parent(),
                                KeyCode::Char('.') => app.filebrowser_toggle_hidden(),
                                KeyCode::Esc => {
                                    app.compose_field = ComposeField::Body;
                                }
                                _ => {}
                            },
                            ComposeField::To | ComposeField::Cc | ComposeField::Bcc => {
                                if app.compose_show_suggestions {
                                    match key.code {
                                        KeyCode::Down => {
                                            if app.compose_suggestion_selected + 1
                                                < app.compose_suggestions.len()
                                            {
                                                app.compose_suggestion_selected += 1;
                                            }
                                        }
                                        KeyCode::Up => {
                                            if app.compose_suggestion_selected > 0 {
                                                app.compose_suggestion_selected -= 1;
                                            }
                                        }
                                        KeyCode::Tab | KeyCode::Enter => {
                                            app.compose_accept_suggestion();
                                        }
                                        KeyCode::Esc => {
                                            app.compose_show_suggestions = false;
                                        }
                                        KeyCode::Char(c) => {
                                            app.compose_address_input(c);
                                        }
                                        KeyCode::Backspace => {
                                            app.compose_address_backspace();
                                        }
                                        _ => {}
                                    }
                                } else {
                                    match key.code {
                                        KeyCode::Esc => {
                                            app.cancel_compose();
                                        }
                                        KeyCode::Char(c) => {
                                            app.compose_address_input(c);
                                        }
                                        KeyCode::Backspace => {
                                            app.compose_address_backspace();
                                        }
                                        KeyCode::Tab => {
                                            app.compose_next_field();
                                        }
                                        KeyCode::BackTab => {
                                            app.compose_prev_field();
                                        }
                                        KeyCode::Down | KeyCode::Enter => {
                                            app.compose_next_field();
                                        }
                                        KeyCode::Up => {
                                            app.compose_prev_field();
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            ComposeField::Subject => match key.code {
                                KeyCode::Esc => {
                                    app.cancel_compose();
                                }
                                KeyCode::Char(c) => {
                                    app.compose_subject.push(c);
                                }
                                KeyCode::Backspace => {
                                    app.compose_subject.pop();
                                }
                                KeyCode::Tab | KeyCode::Down | KeyCode::Enter => {
                                    app.compose_next_field();
                                }
                                KeyCode::BackTab | KeyCode::Up => {
                                    app.compose_prev_field();
                                }
                                _ => {}
                            },
                            ComposeField::Body => match key.code {
                                KeyCode::Esc => {
                                    app.cancel_compose();
                                }
                                KeyCode::Char(c) => {
                                    app.compose_body_insert_char(c);
                                }
                                KeyCode::Enter => {
                                    app.compose_body_newline();
                                }
                                KeyCode::Backspace => {
                                    app.compose_body_backspace();
                                }
                                KeyCode::Delete => {
                                    app.compose_body_delete();
                                }
                                KeyCode::Left => {
                                    app.compose_body_left();
                                }
                                KeyCode::Right => {
                                    app.compose_body_right();
                                }
                                KeyCode::Up => {
                                    app.compose_body_up();
                                }
                                KeyCode::Down => {
                                    app.compose_body_down();
                                }
                                KeyCode::Home => {
                                    app.compose_body_home();
                                }
                                KeyCode::End => {
                                    app.compose_body_end();
                                }
                                KeyCode::BackTab => {
                                    app.compose_prev_field();
                                }
                                _ => {}
                            },
                        }
                    }
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    app.start_selection(mouse.column, mouse.row);
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    app.update_selection(mouse.column, mouse.row);
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    app.update_selection(mouse.column, mouse.row);
                    if let Some(sel) = &app.selection
                        && (sel.start_col != sel.end_col || sel.start_row != sel.end_row)
                    {
                        let text = extract_selection_text(cf.buffer, sel, app.selectable_area);
                        if !text.is_empty() {
                            copy_to_clipboard(&text);
                        }
                    }
                    app.clear_selection();
                }
                MouseEventKind::ScrollUp => match &app.view {
                    ViewMode::FolderSelect => app.folder_tree_up(),
                    ViewMode::List => app.move_up(),
                    ViewMode::Detail => match app.detail_mode {
                        DetailMode::Text => app.scroll_detail_up(),
                        DetailMode::Attachments => {
                            app.prev_attachment();
                            app.update_image_preview(db);
                        }
                        DetailMode::Links => app.prev_link(),
                    },
                    ViewMode::Search => app.search_move_up(),
                    ViewMode::Compose => {
                        if app.compose_field == ComposeField::FileBrowser {
                            app.filebrowser_up();
                        } else if app.compose_body_scroll > 0 {
                            app.compose_body_scroll -= 1;
                        }
                    }
                },
                MouseEventKind::ScrollDown => match &app.view {
                    ViewMode::FolderSelect => app.folder_tree_down(),
                    ViewMode::List => app.move_down(),
                    ViewMode::Detail => match app.detail_mode {
                        DetailMode::Text => app.scroll_detail_down(),
                        DetailMode::Attachments => {
                            app.next_attachment();
                            app.update_image_preview(db);
                        }
                        DetailMode::Links => app.next_link(),
                    },
                    ViewMode::Search => app.search_move_down(),
                    ViewMode::Compose => {
                        if app.compose_field == ComposeField::FileBrowser {
                            app.filebrowser_down();
                        } else {
                            app.compose_body_scroll += 1;
                        }
                    }
                },
                _ => {}
            },
            _ => {}
        }
    }
}

/// Apply one daemon-pushed sync/upload event. Notification delivery is
/// deliberately *not* decided here (or anywhere in this binary) — see
/// `jamail::daemon::dispatch_sync_event`, which runs inside `jamaild` and
/// fires regardless of what this client is displaying or whether it's even
/// running.
fn handle_sync_event(app: &mut App, db: &db::MailDb, ev: ipc::Event) {
    match ev {
        ipc::Event::Syncing { account, folder } => {
            app.spinner_active = true;
            if !app.status_sticky {
                app.status_msg = format!("Syncing {}/{}...", account, folder);
            }
        }
        ipc::Event::Progress {
            account,
            folder,
            done,
            total,
        } => {
            if !app.status_sticky {
                app.status_msg = format!("Syncing {}/{} {}/{}...", account, folder, done, total);
            }
            if done % 20 == 0 || done == total {
                let should_refresh = (account == app.current_account
                    && folder == app.current_folder)
                    || (app.is_global_inbox && folder == "INBOX");
                if should_refresh {
                    app.refresh_emails(db);
                }
            }
        }
        ipc::Event::FolderComplete {
            account,
            folder,
            count,
        } => {
            let should_refresh = (account == app.current_account && folder == app.current_folder)
                || (app.is_global_inbox && folder == "INBOX");
            if count > 0 && should_refresh {
                app.refresh_emails(db);
                if !app.status_sticky {
                    app.status_msg = format!("{}/{}: +{} new", account, folder, count);
                }
            }
        }
        ipc::Event::AllComplete { .. } => {
            app.spinner_active = false;
            if !app.status_sticky && app.status_msg.starts_with("Syncing") {
                app.status_msg.clear();
            }
        }
        ipc::Event::Error { account, message } => {
            app.spinner_active = false;
            if !app.status_sticky {
                app.status_msg = format!("Sync {}: {}", account, message);
            }
        }
        ipc::Event::FlagsChanged { account, folder } => {
            let should_refresh = (account == app.current_account && folder == app.current_folder)
                || (app.is_global_inbox && folder == "INBOX");
            if should_refresh {
                app.refresh_emails(db);
            }
        }
        ipc::Event::FoldersLoaded { account, folders } => {
            app.account_folders.insert(account, folders);
            // Rebuild folder tree if we're on the folder select screen
            if app.view == ViewMode::FolderSelect {
                app.rebuild_folder_tree();
            }
        }
        ipc::Event::UploadComplete { kind, local_id, .. } => {
            app.handle_upload_event(db, kind, local_id, None);
        }
        ipc::Event::UploadError {
            kind,
            local_id,
            message,
            ..
        } => {
            app.handle_upload_event(db, kind, local_id, Some(message));
        }
    }
}

/// Base64-encode `upload.raw_message` and send it to `jamaild` as an
/// `EnqueueUpload` request for `account`.
fn send_upload_request(
    ipc_client: &ipc::IpcClient,
    account: &str,
    upload: jamail::sync::UploadRequest,
) {
    use base64::Engine;
    let raw_message_b64 = base64::engine::general_purpose::STANDARD.encode(&upload.raw_message);
    ipc_client.send(ipc::Request::EnqueueUpload {
        account: account.to_string(),
        kind: upload.kind,
        local_id: upload.local_id,
        folder: upload.folder,
        raw_message_b64,
    });
}

fn extract_selection_text(buffer: &Buffer, sel: &app::Selection, area: Rect) -> String {
    let (sr, sc, er, ec) = sel.normalized();
    let mut result = String::new();

    for row in sr..=er {
        if row < area.y || row >= area.y + area.height {
            continue;
        }
        let col_start = if row == sr { sc } else { area.x };
        let col_end = if row == er {
            ec
        } else {
            area.x + area.width - 1
        };

        let mut line = String::new();
        for col in col_start..=col_end {
            if col < area.x || col >= area.x + area.width {
                continue;
            }
            if let Some(cell) = buffer.cell(Position::new(col, row)) {
                let sym = cell.symbol();
                if sym.is_empty() {
                    continue;
                }
                line.push_str(sym);
            }
        }

        let trimmed = line.trim_end();
        result.push_str(trimmed);
        if row < er {
            result.push('\n');
        }
    }

    while result.ends_with('\n') {
        result.pop();
    }

    result
}

fn handle_compose_send(
    app: &mut App,
    db: &db::MailDb,
    accounts: &[(String, config::JamailAccount)],
    send_queue: &smtp::SendQueue,
) {
    // Look up SMTP config for current account
    let smtp_config = accounts
        .iter()
        .find(|(n, _)| *n == app.current_account)
        .and_then(|(_, acc)| acc.smtp.as_ref());

    let smtp_config = match smtp_config {
        Some(c) => c.clone(),
        None => {
            app.status_msg = "No SMTP config for this account".to_string();
            return;
        }
    };

    // Validate the selected From address against the account's configured
    // sender identities. The From field can only be *set* by cycling
    // through those identities, but this catches any stale/corrupted state
    // before mail ever gets dispatched.
    if !app.compose_from_is_valid() {
        app.status_msg = "Invalid sender address".to_string();
        app.status_sticky = true;
        return;
    }

    // Validate To is non-empty
    if app.compose_to.trim().is_empty() {
        app.status_msg = "To field is empty".to_string();
        return;
    }

    let body = app.compose_body_text();
    let from = app.compose_from.clone();
    let to = app.compose_to.clone();
    let cc = app.compose_cc.clone();
    let bcc = app.compose_bcc.clone();
    let subject = app.compose_subject.clone();
    let reply_msg_id = app.compose_reply_message_id.clone();
    let reply_refs = app.compose_reply_references.clone();

    // Collect attachment paths for the background thread
    let attachments: Vec<smtp::ComposeAttachment> = app
        .compose_attachments
        .iter()
        .map(|a| smtp::ComposeAttachment {
            path: a.path.clone(),
            filename: a.filename.clone(),
            size: a.size,
        })
        .collect();

    // Auto-save as draft before sending, then flip it to "sending" so it
    // stays visible in the Drafts list (with an in-flight indicator)
    // instead of disappearing until the background send completes.
    app.save_compose_as_draft(db);
    let draft_id = app.compose_draft_id;
    if let Some(id) = draft_id {
        let _ = db.mark_draft_sending(id);
        if let Some(vf) = app.virtual_folder.clone() {
            app.load_virtual_folder(db, &vf);
        }
    }
    let sent_folder = app.current_sent_folder();

    send_queue.enqueue(smtp::SendJob {
        account: app.current_account.clone(),
        smtp_config,
        from,
        to,
        cc,
        bcc,
        subject,
        body,
        attachments,
        in_reply_to: reply_msg_id,
        references: reply_refs,
        draft_id,
        sent_folder,
    });

    app.status_msg = "Queued for sending".to_string();
    app.status_sticky = true;
    app.view = app.compose_previous_view.clone();
}

fn enqueue_mark_seen(app: &App, db: &db::MailDb, ipc_client: &ipc::IpcClient) {
    if let Some(id) = app.detail_id
        && let Ok(Some((account, uid, folder))) = db.get_email_account_uid_and_folder(id)
    {
        ipc_client.send(ipc::Request::MarkSeen {
            account,
            folder,
            uid,
        });
    }
}

fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let commands: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];

    for (cmd, args) in commands {
        if let Ok(mut child) = Command::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            return;
        }
    }
}
