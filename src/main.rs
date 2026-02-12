mod app;
mod config;
mod db;
mod mail;
mod sync;
#[allow(dead_code)]
mod theme;

use anyhow::{Context, Result};
use app::{App, ViewMode};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use std::io::{self, IsTerminal};
use std::sync::mpsc;
use std::time::Duration;

fn main() -> Result<()> {
    if !io::stdout().is_terminal() {
        anyhow::bail!("jamail requires an interactive terminal (TTY) to run");
    }

    // Load config
    let config = config::HimalayaConfig::load().context("Failed to load himalaya config")?;
    let (name, account) = config.default_account()?;

    // Open database (creates on first run)
    let db = db::MailDb::open().context("Failed to open mail database")?;

    // Load cached emails immediately (may be empty on first run)
    let emails = db.get_email_list().context("Failed to load emails from database")?;

    // Spawn background sync thread
    let (sync_tx, sync_rx) = mpsc::channel();
    sync::spawn_sync_thread(account.clone(), name.to_string(), sync_tx);

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run app
    let mut app = App::new(emails);
    let result = run_app(&mut terminal, &mut app, &db, &sync_rx);

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(err) = result {
        eprintln!("Error: {:?}", err);
    }

    Ok(())
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    db: &db::MailDb,
    sync_rx: &mpsc::Receiver<sync::SyncEvent>,
) -> Result<()> {
    loop {
        terminal.draw(|frame| app.render(frame))?;

        // Drain sync events (non-blocking)
        while let Ok(ev) = sync_rx.try_recv() {
            match ev {
                sync::SyncEvent::Syncing => {
                    app.status_msg = "Syncing...".to_string();
                }
                sync::SyncEvent::Progress(done, total) => {
                    app.status_msg = format!("Syncing {}/{}...", done, total);
                    if done % 20 == 0 || done == total {
                        app.refresh_emails(db);
                    }
                }
                sync::SyncEvent::Complete(count) => {
                    if count > 0 {
                        app.refresh_emails(db);
                        app.status_msg = format!("+{} new", count);
                    } else {
                        app.status_msg.clear();
                    }
                }
                sync::SyncEvent::Error(e) => {
                    app.status_msg = format!("Sync: {}", e);
                }
            }
        }

        // Poll for keyboard events with 200ms timeout
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match &app.view {
                ViewMode::List => match key.code {
                    KeyCode::Char('q') => {
                        app.should_quit = true;
                        return Ok(());
                    }
                    KeyCode::Char('j') | KeyCode::Down => app.move_down(),
                    KeyCode::Char('k') | KeyCode::Up => app.move_up(),
                    KeyCode::Enter => app.open_detail(db),
                    KeyCode::Char('/') => app.enter_search(),
                    KeyCode::Char('g') => app.selected = 0,
                    KeyCode::Char('G') => {
                        if !app.emails.is_empty() {
                            app.selected = app.emails.len() - 1;
                        }
                    }
                    _ => {}
                },
                ViewMode::Detail => match key.code {
                    KeyCode::Char('q') => {
                        app.should_quit = true;
                        return Ok(());
                    }
                    KeyCode::Esc | KeyCode::Backspace => app.close_detail(),
                    KeyCode::Char('j') | KeyCode::Down => app.scroll_detail_down(),
                    KeyCode::Char('k') | KeyCode::Up => app.scroll_detail_up(),
                    KeyCode::Char(' ') | KeyCode::PageDown => {
                        for _ in 0..10 {
                            app.scroll_detail_down();
                        }
                    }
                    KeyCode::Char('v') => app.open_in_browser(),
                    _ => {}
                },
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
    }
}
