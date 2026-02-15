mod app;
mod config;
mod db;
mod mail;
mod sync;
#[allow(dead_code)]
mod theme;
mod thread;

use anyhow::{Context, Result};
use app::{App, DetailMode, ViewMode};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::prelude::*;
use ratatui_image::picker::Picker;
use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

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
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Detect terminal graphics protocol for image previews
    let picker = Picker::from_query_stdio().ok();

    // Run app
    let mut app = App::new(
        emails,
        picker,
        accounts.clone(),
        default_name.clone(),
        current_folder,
    );

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
    );

    // Restore terminal
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
) -> Result<()> {
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
                    app.status_msg = format!("Syncing {}...", folder);
                }
                sync::SyncEvent::Progress(folder, done, total) => {
                    app.status_msg = format!("Syncing {} {}/{}...", folder, done, total);
                    if done % 20 == 0 || done == total {
                        // Only refresh if the syncing folder matches current view
                        if folder == app.current_folder {
                            app.refresh_emails(db);
                        }
                    }
                }
                sync::SyncEvent::FolderComplete(folder, count) => {
                    if count > 0 && folder == app.current_folder {
                        app.refresh_emails(db);
                        app.status_msg = format!("{}: +{} new", folder, count);
                    }
                }
                sync::SyncEvent::AllComplete => {
                    if app.status_msg.starts_with("Syncing") {
                        app.status_msg.clear();
                    }
                }
                sync::SyncEvent::Error(e) => {
                    app.status_msg = format!("Sync: {}", e);
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

        // Poll for events with 200ms timeout
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                app.clear_selection();

                match &app.view {
                    ViewMode::FolderSelect => match key.code {
                        KeyCode::Char('q') => {
                            app.should_quit = true;
                            return Ok(());
                        }
                        KeyCode::Esc => {
                            app.view = ViewMode::List;
                        }
                        KeyCode::Up => app.folder_tree_up(),
                        KeyCode::Down => app.folder_tree_down(),
                        KeyCode::Tab => app.folder_tree_toggle_expand(),
                        KeyCode::Enter => {
                            if let Some((acct_name, folder_name)) = app.folder_tree_select() {
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
                        _ => {}
                    },
                    ViewMode::List => {
                        let page = terminal
                            .size()
                            .map(|s| s.height.saturating_sub(2) as usize)
                            .unwrap_or(20);
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
                            KeyCode::Enter => app.open_detail(db),
                            KeyCode::Char(' ') | KeyCode::Char('l') => app.toggle_thread_expand(),
                            KeyCode::Tab => app.next_thread(),
                            KeyCode::BackTab => app.prev_thread(),
                            KeyCode::Char('t') => app.toggle_thread_mode(),
                            KeyCode::Char('/') => app.enter_search(),
                            KeyCode::Char('F') => app.enter_folder_select(),
                            _ => {}
                        }
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
                                DetailMode::Text => app.prev_in_list(db),
                                DetailMode::Attachments => {
                                    app.prev_attachment();
                                    app.update_image_preview(db);
                                }
                                DetailMode::Links => app.prev_link(),
                            },
                            KeyCode::Right => match app.detail_mode {
                                DetailMode::Text => app.next_in_list(db),
                                DetailMode::Attachments => {
                                    app.next_attachment();
                                    app.update_image_preview(db);
                                }
                                DetailMode::Links => app.next_link(),
                            },
                            KeyCode::Enter => {
                                if app.detail_mode == DetailMode::Links {
                                    app.open_selected_link();
                                }
                            }
                            KeyCode::Char('s') => {
                                if app.detail_mode == DetailMode::Attachments {
                                    app.save_attachment(db);
                                }
                            }
                            KeyCode::Char('/') => {
                                app.cycle_detail_mode();
                                app.update_image_preview(db);
                            }
                            KeyCode::Char(' ') | KeyCode::PageDown => {
                                app.scroll_detail_page_down(page)
                            }
                            KeyCode::PageUp => app.scroll_detail_page_up(page),
                            KeyCode::Home => app.scroll_detail_home(),
                            KeyCode::End => app.scroll_detail_end(),
                            KeyCode::Char('n') => app.next_in_thread(db),
                            KeyCode::Char('p') => app.prev_in_thread(db),
                            KeyCode::Char('h') => app.toggle_raw_headers(),
                            KeyCode::Char('v') => app.open_in_browser(),
                            _ => {}
                        }
                    }
                    ViewMode::Search => match key.code {
                        KeyCode::Esc => app.exit_search(),
                        KeyCode::Enter => {
                            if !app.search_results.is_empty() {
                                app.open_detail(db);
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
