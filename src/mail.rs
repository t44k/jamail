use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDateTime};
use imap::Connection;
use mailparse::{parse_mail, MailHeaderMap};

use crate::config::Account;

pub struct Email {
    pub uid: u32,
    pub from: String,
    pub subject: String,
    pub date: DateTime<Local>,
    pub is_unread: bool,
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

    pub fn fetch_inbox(&mut self, count: u32) -> Result<Vec<Email>> {
        let mailbox = self.session.select("INBOX").context("Failed to select INBOX")?;
        let total = mailbox.exists;
        if total == 0 {
            return Ok(Vec::new());
        }

        let start = if total > count { total - count + 1 } else { 1 };
        let range = format!("{}:{}", start, total);

        let messages = self
            .session
            .fetch(&range, "(UID FLAGS ENVELOPE BODY.PEEK[TEXT]<0.200>)")
            .context("Failed to fetch messages")?;

        let mut emails: Vec<Email> = Vec::new();

        for msg in messages.iter() {
            let uid = msg.uid.unwrap_or(0);
            let is_unread = !msg
                .flags()
                .iter()
                .any(|f| matches!(f, imap::types::Flag::Seen));

            let (from, subject, date) = if let Some(env) = msg.envelope() {
                let from = env
                    .from
                    .as_ref()
                    .and_then(|addrs: &Vec<_>| addrs.first())
                    .map(|addr| {
                        if let Some(name) = &addr.name {
                            decode_mime_str(&String::from_utf8_lossy(name))
                        } else if let Some(mbox) = &addr.mailbox {
                            let mbox = String::from_utf8_lossy(mbox);
                            if let Some(host) = &addr.host {
                                format!("{}@{}", mbox, String::from_utf8_lossy(host))
                            } else {
                                mbox.to_string()
                            }
                        } else {
                            "(unknown)".to_string()
                        }
                    })
                    .unwrap_or_else(|| "(unknown)".to_string());

                let subject = env
                    .subject
                    .as_ref()
                    .map(|s| decode_mime_str(&String::from_utf8_lossy(s)))
                    .unwrap_or_else(|| "(no subject)".to_string());

                let date = env
                    .date
                    .as_ref()
                    .and_then(|d| parse_imap_date(&String::from_utf8_lossy(d)))
                    .unwrap_or_else(Local::now);

                (from, subject, date)
            } else {
                ("(unknown)".to_string(), "(no subject)".to_string(), Local::now())
            };

            let preview = msg
                .text()
                .map(|t| {
                    let text = String::from_utf8_lossy(t);
                    text.chars()
                        .filter(|c| !c.is_control())
                        .take(100)
                        .collect::<String>()
                })
                .unwrap_or_default();

            emails.push(Email {
                uid,
                from,
                subject,
                date,
                is_unread,
                preview,
            });
        }

        emails.sort_by(|a, b| b.date.cmp(&a.date));
        Ok(emails)
    }

    pub fn fetch_email_content(&mut self, uid: u32) -> Result<EmailContent> {
        let messages = self
            .session
            .uid_fetch(uid.to_string(), "(BODY[] FLAGS)")
            .context("Failed to fetch email content")?;

        let msg = messages.iter().next().context("Email not found")?;

        // Mark as seen
        let _ = self.session.uid_store(uid.to_string(), "+FLAGS (\\Seen)");

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

        // If we only have HTML, convert to text via w3m
        if text_body.is_empty() {
            if let Some(html) = &html_body {
                text_body = html_to_text(html);
            }
        }

        Ok(EmailContent {
            from,
            to,
            subject,
            date,
            text_body,
            html_body,
        })
    }

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
