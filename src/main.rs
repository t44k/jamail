mod app;
mod config;
mod db;
mod mail;
mod smtp;
mod sync;
#[allow(dead_code)]
mod theme;
mod thread;

use anyhow::{Context, Result};
use app::{App, ComposeField, DetailMode, ViewMode};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement},
};
use ratatui::prelude::*;
use ratatui_image::picker::Picker;
use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    if !io::stdout().is_terminal() {
        anyhow::bail!("jamail requires an interactive terminal (TTY) to run");
    }

    // Load config
    let config = config::JamailConfig::load().context("Failed to load jamail config")?;
    let (default_name, _default_account) = config.default_account()?;
    let default_name = default_name.to_string();

    let accounts: Vec<(String, config::JamailAccount)> = config.accounts.into_iter().collect();

    let account = accounts
        .iter()
        .find(|(n, _)| *n == default_name)
        .map(|(_, a)| a.clone())
        .context("Default account not found")?;

    // Open database (creates on first run)
    let db = db::MailDb::open().context("Failed to open mail database")?;

    let current_folder = "INBOX".to_string();

    // Load cached emails immediately
    let emails = db
        .get_email_list(&default_name, &current_folder)
        .context("Failed to load emails from database")?;

    // Load cached folders
    let cached_folders = db.get_folders(&default_name).unwrap_or_default();

    // Spawn background sync thread
    let (sync_tx, sync_rx) = mpsc::channel();
    let sync_control = Arc::new(sync::SyncControl::new(&current_folder));
    let _sync_handle = sync::spawn_sync_thread(
        account,
        default_name.clone(),
        Arc::clone(&sync_control),
        sync_tx,
    );

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
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
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
        sync_rx,
        &sync_control,
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
    sync_rx: mpsc::Receiver<sync::SyncEvent>,
    sync_control: &Arc<sync::SyncControl>,
    accounts: &[(String, config::JamailAccount)],
    send_queue: smtp::SendQueue,
) -> Result<()> {
    // Track time between loop iterations to detect sleep/wake
    let mut last_loop_time = Instant::now();

    // Track current sync control so we can shut it down on account switch
    let mut current_sync_control = Arc::clone(sync_control);
    // Keep sender alive to prevent channel from closing; receiver is swapped on account switch
    #[allow(unused_assignments)]
    let mut _sync_tx_keepalive: Option<mpsc::Sender<sync::SyncEvent>> = None;
    let mut current_sync_rx = sync_rx;

    loop {
        let cf = terminal.draw(|frame| {
            app.render(frame);
            app.render_selection(frame);
        })?;

        // Drain sync events (non-blocking)
        while let Ok(ev) = current_sync_rx.try_recv() {
            match ev {
                sync::SyncEvent::Syncing(folder) => {
                    app.spinner_active = true;
                    if !app.status_sticky {
                        app.status_msg = format!("Syncing {}...", folder);
                    }
                }
                sync::SyncEvent::Progress(folder, done, total) => {
                    if !app.status_sticky {
                        app.status_msg = format!("Syncing {} {}/{}...", folder, done, total);
                    }
                    if done % 20 == 0 || done == total {
                        let should_refresh = folder == app.current_folder
                            || (app.is_global_inbox && folder == "INBOX");
                        if should_refresh {
                            app.refresh_emails(db);
                        }
                    }
                }
                sync::SyncEvent::FolderComplete(folder, count) => {
                    let should_refresh = folder == app.current_folder
                        || (app.is_global_inbox && folder == "INBOX");
                    if count > 0 && should_refresh {
                        app.refresh_emails(db);
                        if !app.status_sticky {
                            app.status_msg = format!("{}: +{} new", folder, count);
                        }
                    }
                }
                sync::SyncEvent::AllComplete => {
                    app.spinner_active = false;
                    if !app.status_sticky && app.status_msg.starts_with("Syncing") {
                        app.status_msg.clear();
                    }
                }
                sync::SyncEvent::Error(e) => {
                    app.spinner_active = false;
                    if !app.status_sticky {
                        app.status_msg = format!("Sync: {}", e);
                    }
                }
                sync::SyncEvent::FlagsChanged(folder) => {
                    let should_refresh = folder == app.current_folder
                        || (app.is_global_inbox && folder == "INBOX");
                    if should_refresh {
                        app.refresh_emails(db);
                    }
                }
                sync::SyncEvent::FoldersLoaded(folders) => {
                    app.account_folders
                        .insert(app.current_account.clone(), folders);
                    // Rebuild folder tree if we're on the folder select screen
                    if app.view == ViewMode::FolderSelect {
                        app.rebuild_folder_tree();
                    }
                }
            }
        }

        // Drain completed send-queue results.
        while let Ok(result) = send_queue.result_rx.try_recv() {
            match result.error {
                None => {
                    // Mark draft as sent in DB
                    if let Some(id) = result.draft_id {
                        let _ = db.mark_draft_sent(id);
                    }
                    app.last_send_error = None;
                    if !app.status_sticky {
                        app.status_msg = "Message sent!".to_string();
                        app.status_sticky = true;
                    }
                }
                Some(e) => {
                    app.last_send_error = Some(e.clone());
                    app.status_msg = format!("Send failed: {}", e);
                    app.status_sticky = true;
                }
            }
        }

        // Poll for events with 200ms timeout
        if !event::poll(Duration::from_millis(200))? {
            // Check for sleep/wake: if the loop took much longer than 200ms
            let loop_elapsed = last_loop_time.elapsed();
            last_loop_time = Instant::now();
            if loop_elapsed > Duration::from_secs(70) {
                current_sync_control
                    .force_reconnect
                    .store(true, Ordering::Relaxed);
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
                                    app.is_global_inbox = false;
                                    app.virtual_folder = None;
                                    app.local_messages.clear();
                                    let needs_account_switch = acct_name != app.current_account;

                                    app.current_folder = folder_name.clone();

                                    if needs_account_switch {
                                        // Shut down old sync thread
                                        current_sync_control.shutdown.store(true, Ordering::Relaxed);

                                        app.current_account = acct_name.clone();

                                        // Load emails from cache for new account+folder
                                        if let Ok(emails) =
                                            db.get_email_list(&app.current_account, &app.current_folder)
                                        {
                                            app.emails = emails;
                                            let expanded =
                                                std::mem::take(&mut app.threaded_view.expanded);
                                            app.threaded_view.threads =
                                                thread::build_threads(&app.emails);
                                            app.threaded_view.expanded = expanded;
                                            thread::rebuild_rows(&mut app.threaded_view);
                                            app.selected = 0;
                                            app.list_scroll_offset = 0;
                                        }

                                        // Spawn new sync thread
                                        if let Some((_, new_account)) =
                                            accounts.iter().find(|(n, _)| *n == acct_name)
                                        {
                                            let new_control =
                                                Arc::new(sync::SyncControl::new(&folder_name));
                                            let (tx, rx) = mpsc::channel();
                                            let _handle = sync::spawn_sync_thread(
                                                new_account.clone(),
                                                acct_name.clone(),
                                                Arc::clone(&new_control),
                                                tx.clone(),
                                            );
                                            current_sync_control = new_control;
                                            // Keep sender alive to prevent channel close
                                            _sync_tx_keepalive = Some(tx);
                                            current_sync_rx = rx;
                                        }
                                    } else {
                                        // Same account, just switch folder
                                        if let Ok(mut cf) = current_sync_control.current_folder.write()
                                        {
                                            *cf = folder_name;
                                        }

                                        // Load emails from cache immediately
                                        app.refresh_emails(db);
                                        app.selected = 0;
                                        app.list_scroll_offset = 0;
                                    }

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
                                    enqueue_mark_seen(app, db, &current_sync_control);
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
                                DetailMode::Text => app.scroll_detail_down(),
                                DetailMode::Attachments => {
                                    app.next_attachment();
                                    app.update_image_preview(db);
                                }
                                DetailMode::Links => app.next_link(),
                            },
                            KeyCode::Up => match app.detail_mode {
                                DetailMode::Text => app.scroll_detail_up(),
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
                                        enqueue_mark_seen(app, db, &current_sync_control);
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
                                        enqueue_mark_seen(app, db, &current_sync_control);
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
                                DetailMode::Links => app.open_selected_link(),
                                DetailMode::Attachments => app.open_attachment(db),
                                DetailMode::Text => {}
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
                                    enqueue_mark_seen(app, db, &current_sync_control);
                                } else {
                                    app.scroll_detail_page_down(page);
                                }
                            }
                            KeyCode::PageUp => {
                                if key.modifiers.contains(KeyModifiers::SHIFT) {
                                    app.prev_in_thread(db);
                                    enqueue_mark_seen(app, db, &current_sync_control);
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
                            _ => {}
                        }
                    }
                    ViewMode::Search => match key.code {
                        KeyCode::Esc | KeyCode::Left => app.exit_search(),
                        KeyCode::Enter | KeyCode::Right => {
                            if !app.search_results.is_empty() {
                                app.open_detail(db);
                                enqueue_mark_seen(app, db, &current_sync_control);
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
                            app.save_compose_as_draft(db);
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

    // Auto-save as draft before sending
    app.save_compose_as_draft(db);
    let draft_id = app.compose_draft_id;

    send_queue.enqueue(smtp::SendJob {
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
    });

    app.status_msg = "Queued for sending".to_string();
    app.status_sticky = true;
    app.view = app.compose_previous_view.clone();
}

fn enqueue_mark_seen(app: &App, db: &db::MailDb, sync_control: &sync::SyncControl) {
    if let Some(id) = app.detail_id
        && let Ok(Some((uid, folder))) = db.get_email_uid_and_folder(id)
        && let Ok(mut queue) = sync_control.mark_seen_queue.lock()
    {
        queue.push(sync::MarkSeenRequest { folder, uid });
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
