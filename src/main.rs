mod app;
mod config;
mod mail;
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

fn main() -> Result<()> {
    if !io::stdout().is_terminal() {
        anyhow::bail!("jamail requires an interactive terminal (TTY) to run");
    }

    // Load config
    let config = config::HimalayaConfig::load().context("Failed to load himalaya config")?;
    let (name, account) = config.default_account()?;
    eprintln!("Connecting to account: {} ({})", name, account.email);

    // Connect to IMAP
    let mut client =
        mail::MailClient::connect(account).context("Failed to connect to IMAP server")?;
    eprintln!("Connected. Fetching emails...");

    // Fetch emails
    let emails = client.fetch_inbox(50).context("Failed to fetch inbox")?;
    eprintln!("Loaded {} emails.", emails.len());

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run app
    let mut app = App::new(emails);
    let result = run_app(&mut terminal, &mut app, &mut client);

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    // Logout
    client.logout();

    if let Err(err) = result {
        eprintln!("Error: {:?}", err);
    }

    Ok(())
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    client: &mut mail::MailClient,
) -> Result<()> {
    loop {
        terminal.draw(|frame| app.render(frame))?;

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
                    KeyCode::Enter => app.open_detail(client),
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
            }
        }
    }
}
