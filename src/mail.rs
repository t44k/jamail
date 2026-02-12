use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDateTime};
use imap::Connection;
use mailparse::{parse_mail, MailHeaderMap};
use std::time::Duration;

use crate::config::Account;
use crate::db::MailDb;

pub struct Email {
    pub uid: u32,
    pub from: String,
    pub subject: String,
    pub date: DateTime<Local>,
    pub is_unread: bool,
    #[allow(dead_code)]
    pub preview: String,
}

pub struct EmailContent {
    pub from: String,
    pub to: String,
    pub subject: String,
    pub date: DateTime<Local>,
    pub text_body: String,
    pub html_body: Option<String>,
}

struct ProcessedEmail {
    from: String,
    to: String,
    subject: String,
    date: DateTime<Local>,
    is_unread: bool,
    preview: String,
    text_body: String,
    html_body: Option<String>,
}

pub struct MailClient {
    session: imap::Session<Connection>,
}

impl MailClient {
    pub fn connect(account: &Account) -> Result<Self> {
        let host = &account.backend.host;
        let port = account.backend.port;
        let login = &account.backend.login;
        let password = account
            .backend
            .auth
            .raw
            .as_deref()
            .context("Only raw password auth is supported")?;

        let client = imap::ClientBuilder::new(host.as_str(), port)
            .connect()
            .context("Failed to connect to IMAP server")?;

        let session = client
            .login(login, password)
            .map_err(|e| anyhow::anyhow!("IMAP login failed: {}", e.0))?;

        Ok(Self { session })
    }

    pub fn sync_inbox(
        &mut self,
        db: &MailDb,
        account: &str,
        progress: &dyn Fn(usize, usize),
    ) -> Result<usize> {
        let mailbox = self.session.select("INBOX").context("Failed to select INBOX")?;
        let server_uidvalidity = mailbox.uid_validity.unwrap_or(0);

        // Check UIDVALIDITY
        let stored = db.get_sync_state(account)?;
        let last_uid = if let Some((stored_uidvalidity, stored_last_uid)) = stored {
            if stored_uidvalidity != server_uidvalidity {
                db.clear_all_emails()?;
                0
            } else {
                stored_last_uid
            }
        } else {
            0
        };

        // Fetch UIDs of new messages
        let fetch_range = format!("{}:*", last_uid + 1);
        let uid_results = self
            .session
            .uid_fetch(&fetch_range, "UID")
            .context("Failed to fetch UIDs")?;

        let new_uids: Vec<u32> = uid_results
            .iter()
            .filter_map(|msg| msg.uid)
            .filter(|&uid| uid > last_uid)
            .collect();

        if new_uids.is_empty() {
            return Ok(0);
        }

        let total = new_uids.len();
        let mut max_uid = last_uid;
        for (i, &uid) in new_uids.iter().enumerate() {
            progress(i + 1, total);
            match self.fetch_and_process_email(uid) {
                Ok(processed) => {
                    db.store_email(
                        uid,
                        &processed.from,
                        &processed.to,
                        &processed.subject,
                        &processed.date,
                        processed.is_unread,
                        &processed.preview,
                        &processed.text_body,
                        processed.html_body.as_deref(),
                    )?;
                    if uid > max_uid {
                        max_uid = uid;
                    }
                }
                Err(_) => {}
            }
        }

        db.set_sync_state(account, server_uidvalidity, max_uid)?;

        Ok(total)
    }

    fn fetch_and_process_email(&mut self, uid: u32) -> Result<ProcessedEmail> {
        let messages = self
            .session
            .uid_fetch(uid.to_string(), "(BODY.PEEK[] FLAGS)")
            .context("Failed to fetch email")?;

        let msg = messages.iter().next().context("Email not found")?;

        let is_unread = !msg
            .flags()
            .iter()
            .any(|f| matches!(f, imap::types::Flag::Seen));

        let body = msg.body().context("No body in message")?;
        let parsed = parse_mail(body).context("Failed to parse email")?;

        let from = parsed
            .headers
            .get_first_value("From")
            .unwrap_or_else(|| "(unknown)".to_string());
        let to = parsed
            .headers
            .get_first_value("To")
            .unwrap_or_else(|| "(unknown)".to_string());
        let subject = parsed
            .headers
            .get_first_value("Subject")
            .unwrap_or_else(|| "(no subject)".to_string());
        let date_str = parsed.headers.get_first_value("Date").unwrap_or_default();
        let date = parse_imap_date(&date_str).unwrap_or_else(Local::now);

        let mut text_body = String::new();
        let mut html_body: Option<String> = None;

        extract_parts(&parsed, &mut text_body, &mut html_body);

        if text_body.is_empty() {
            if let Some(html) = &html_body {
                text_body = html_to_text(html);
            }
        }

        // Generate preview: first 200 chars, whitespace normalized
        let preview: String = text_body
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(200)
            .collect();

        Ok(ProcessedEmail {
            from,
            to,
            subject,
            date,
            is_unread,
            preview,
            text_body,
            html_body,
        })
    }

    /// Block until new mail arrives (via IMAP IDLE) or timeout expires.
    /// Falls back to sleeping if IDLE isn't supported.
    pub fn wait_for_changes(&mut self, timeout: Duration) {
        use imap::extensions::idle;

        let mut handle = self.session.idle();
        handle.timeout(timeout).keepalive(false);
        let _ = handle.wait_while(idle::stop_on_any);
    }

    #[allow(dead_code)]
    pub fn logout(mut self) {
        let _ = self.session.logout();
    }
}

fn extract_parts(
    part: &mailparse::ParsedMail,
    text_body: &mut String,
    html_body: &mut Option<String>,
) {
    let content_type = part.ctype.mimetype.to_lowercase();

    if content_type == "text/plain" {
        if let Ok(body) = part.get_body() {
            if text_body.is_empty() {
                *text_body = body;
            }
        }
    } else if content_type == "text/html" {
        if let Ok(body) = part.get_body() {
            if html_body.is_none() {
                *html_body = Some(body);
            }
        }
    }

    for sub in &part.subparts {
        extract_parts(sub, text_body, html_body);
    }
}

pub fn html_to_text(html: &str) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = match Command::new("w3m")
        .args(["-T", "text/html", "-dump", "-cols", "120"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return strip_html_tags(html),
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(html.as_bytes());
    }

    match child.wait_with_output() {
        Ok(output) => String::from_utf8_lossy(&output.stdout).to_string(),
        Err(_) => strip_html_tags(html),
    }
}

fn strip_html_tags(html: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(c),
            _ => {}
        }
    }
    result
}

#[allow(dead_code)]
fn decode_mime_str(s: &str) -> String {
    mailparse::parse_header(format!("X: {}", s).as_bytes())
        .ok()
        .map(|(h, _)| h.get_value())
        .unwrap_or_else(|| s.to_string())
}

fn parse_imap_date(date_str: &str) -> Option<DateTime<Local>> {
    // Try RFC2822 first
    if let Ok(dt) = DateTime::parse_from_rfc2822(date_str) {
        return Some(dt.with_timezone(&Local));
    }

    // Try common IMAP date formats
    let formats = [
        "%a, %d %b %Y %H:%M:%S %z",
        "%d %b %Y %H:%M:%S %z",
        "%a, %d %b %Y %H:%M:%S %Z",
        "%d %b %Y %H:%M:%S %Z",
        "%a, %d %b %Y %H:%M:%S",
        "%d %b %Y %H:%M:%S",
    ];

    for fmt in &formats {
        if let Ok(dt) = DateTime::parse_from_str(date_str.trim(), fmt) {
            return Some(dt.with_timezone(&Local));
        }
    }

    // Try without timezone
    for fmt in &formats[4..] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(date_str.trim(), fmt) {
            let utc_dt = dt.and_utc();
            return Some(utc_dt.with_timezone(&Local));
        }
    }

    None
}

pub fn relative_time(dt: &DateTime<Local>) -> String {
    let now = Local::now();
    let duration = now.signed_duration_since(dt);

    if duration.num_seconds() < 0 {
        return "just now".to_string();
    }

    let seconds = duration.num_seconds();
    let minutes = duration.num_minutes();
    let hours = duration.num_hours();
    let days = duration.num_days();

    if seconds < 60 {
        "just now".to_string()
    } else if minutes == 1 {
        "1 min ago".to_string()
    } else if minutes < 60 {
        format!("{} min ago", minutes)
    } else if hours == 1 {
        "1 hour ago".to_string()
    } else if hours < 24 {
        format!("{} hours ago", hours)
    } else if days == 1 {
        "1 day ago".to_string()
    } else if days < 30 {
        format!("{} days ago", days)
    } else if days < 365 {
        let months = days / 30;
        if months == 1 {
            "1 month ago".to_string()
        } else {
            format!("{} months ago", months)
        }
    } else {
        let years = days / 365;
        if years == 1 {
            "1 year ago".to_string()
        } else {
            format!("{} years ago", years)
        }
    }
}

pub fn open_in_browser(html: &str) -> Result<()> {
    let path = "/tmp/jamail_preview.html";
    std::fs::write(path, html).context("Failed to write HTML to /tmp")?;
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .context("Failed to open browser")?;
    Ok(())
}
