use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDateTime};
use imap::Connection;
use mailparse::{DispositionType, MailHeaderMap, parse_mail};
use std::time::Duration;

use crate::config::JamailAccount;
use crate::db::MailDb;

#[derive(Clone)]
pub struct Email {
    pub id: i64,
    pub uid: u32,
    pub from: String,
    pub subject: String,
    pub date: DateTime<Local>,
    pub is_unread: bool,
    #[allow(dead_code)]
    pub preview: String,
    pub message_id: String,
    pub in_reply_to: String,
    pub references: String,
    pub has_attachments: bool,
}

pub struct AttachmentData {
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: usize,
    pub data: Vec<u8>,
}

pub struct EmailContent {
    pub from: String,
    pub to: String,
    pub subject: String,
    pub date: DateTime<Local>,
    pub text_body: String,
    pub html_body: Option<String>,
    pub raw_headers: String,
}

#[derive(Clone, Debug)]
pub struct FolderInfo {
    pub name: String,
    pub delimiter: String,
}

pub struct ProcessedEmail {
    pub from: String,
    pub to: String,
    pub subject: String,
    pub date: DateTime<Local>,
    pub is_unread: bool,
    pub preview: String,
    pub text_body: String,
    pub html_body: Option<String>,
    pub raw_headers: String,
    pub message_id: String,
    pub in_reply_to: String,
    pub references: String,
    pub has_attachments: bool,
    pub attachments: Vec<AttachmentData>,
}

const FETCH_BATCH_SIZE: usize = 50;
/// Max concurrent process_raw_email threads per batch (limits w3m subprocess count).
const PROCESS_PARALLELISM: usize = 8;

pub struct MailClient {
    session: imap::Session<Connection>,
}

impl MailClient {
    pub fn connect(account: &JamailAccount) -> Result<Self> {
        let host = &account.imap.host;
        let port = account.imap.port;
        let login = &account.imap.login;
        let password = account.imap.auth.resolve_password()?;

        let client = imap::ClientBuilder::new(host.as_str(), port)
            .connect()
            .context("Failed to connect to IMAP server")?;

        let session = client
            .login(login, &password)
            .map_err(|e| anyhow::anyhow!("IMAP login failed: {}", e.0))?;

        Ok(Self { session })
    }

    /// List all folders on the IMAP server, filtering out \Noselect folders.
    pub fn list_folders(&mut self) -> Result<Vec<FolderInfo>> {
        let folders = self
            .session
            .list(Some(""), Some("*"))
            .context("Failed to list IMAP folders")?;

        let mut result = Vec::new();
        for folder in folders.iter() {
            // Skip \Noselect folders
            let is_noselect = folder
                .attributes()
                .iter()
                .any(|a| matches!(a, imap_proto::NameAttribute::NoSelect));
            if is_noselect {
                continue;
            }
            let delimiter = folder
                .delimiter()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "/".to_string());
            result.push(FolderInfo {
                name: folder.name().to_string(),
                delimiter,
            });
        }
        Ok(result)
    }

    pub fn sync_folder(
        &mut self,
        db: &MailDb,
        account: &str,
        folder: &str,
        progress: &dyn Fn(usize, usize),
    ) -> Result<usize> {
        let mailbox = self
            .session
            .select(folder)
            .with_context(|| format!("Failed to select folder: {}", folder))?;
        let server_uidvalidity = mailbox.uid_validity.unwrap_or(0);

        // Check UIDVALIDITY
        let stored = db.get_sync_state(account, folder)?;
        let last_uid = if let Some((stored_uidvalidity, stored_last_uid)) = stored {
            if stored_uidvalidity != server_uidvalidity {
                db.clear_folder_emails(account, folder)?;
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

        // Pre-filter UIDs already in the database
        let uids_to_fetch: Vec<u32> = new_uids
            .iter()
            .copied()
            .filter(|&uid| !db.has_email(account, folder, uid))
            .collect();

        let total = new_uids.len();
        let mut fetched = 0;
        let mut progress_offset = 0;

        // Process in batches
        for chunk in uids_to_fetch.chunks(FETCH_BATCH_SIZE) {
            let raw_batch = self.fetch_raw_batch(chunk)?;

            // Process and store in sub-batches to limit concurrent w3m
            // processes and peak memory (attachment data freed between sub-batches).
            db.begin_tx()?;
            let batch_result = (|| -> Result<usize> {
                let mut batch_stored = 0;
                for sub_chunk in raw_batch.chunks(PROCESS_PARALLELISM) {
                    let sub_processed: Vec<(u32, ProcessedEmail)> =
                        std::thread::scope(|s| {
                            let handles: Vec<_> = sub_chunk
                                .iter()
                                .map(|(uid, is_unread, body)| {
                                    let uid = *uid;
                                    let is_unread = *is_unread;
                                    s.spawn(move || -> Option<(u32, ProcessedEmail)> {
                                        process_raw_email(body, is_unread)
                                            .ok()
                                            .map(|p| (uid, p))
                                    })
                                })
                                .collect();
                            handles
                                .into_iter()
                                .filter_map(|h| h.join().ok().flatten())
                                .collect()
                        });
                    for (uid, processed) in &sub_processed {
                        db.store_email_core(
                            account,
                            folder,
                            *uid,
                            &processed.from,
                            &processed.to,
                            &processed.subject,
                            &processed.date,
                            processed.is_unread,
                            &processed.preview,
                            &processed.text_body,
                            processed.html_body.as_deref(),
                            &processed.raw_headers,
                            &processed.message_id,
                            &processed.in_reply_to,
                            &processed.references,
                            processed.has_attachments,
                            &processed.attachments,
                        )?;
                        batch_stored += 1;
                    }
                    // sub_processed dropped here — frees attachment data
                }
                // Update sync_state to the highest UID in this batch
                if let Some(&max_uid) = chunk.iter().max() {
                    db.set_sync_state(account, folder, server_uidvalidity, max_uid)?;
                }
                Ok(batch_stored)
            })();

            progress_offset += raw_batch.len();
            progress(progress_offset, total);

            match batch_result {
                Ok(n) => {
                    db.commit_tx()?;
                    fetched += n;
                }
                Err(e) => {
                    db.rollback_tx();
                    return Err(e);
                }
            }
        }

        // Report progress for skipped (already-synced) UIDs
        if progress_offset < total {
            progress(total, total);
        }

        // Update sync_state to the absolute highest UID
        if let Some(&max_uid) = new_uids.iter().max() {
            db.set_sync_state(account, folder, server_uidvalidity, max_uid)?;
        }

        Ok(fetched)
    }

    /// Fetch a batch of emails by UID in a single IMAP command.
    /// Returns Vec<(uid, is_unread, raw_body)>.
    fn fetch_raw_batch(&mut self, uids: &[u32]) -> Result<Vec<(u32, bool, Vec<u8>)>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let uid_list: String = uids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
        let messages = self
            .session
            .uid_fetch(&uid_list, "(BODY.PEEK[] FLAGS)")
            .context("Failed to fetch email batch")?;

        let mut results = Vec::with_capacity(uids.len());
        for msg in messages.iter() {
            let uid = match msg.uid {
                Some(u) => u,
                None => continue,
            };
            let is_unread = !msg
                .flags()
                .iter()
                .any(|f| matches!(f, imap::types::Flag::Seen));
            let body = match msg.body() {
                Some(b) => b.to_vec(),
                None => continue,
            };
            results.push((uid, is_unread, body));
        }
        Ok(results)
    }

    /// SELECT a folder on the IMAP server.
    pub fn select_folder(&mut self, folder: &str) -> Result<()> {
        self.session
            .select(folder)
            .with_context(|| format!("Failed to select folder: {}", folder))?;
        Ok(())
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

/// Process a raw email body + flags into a structured ProcessedEmail.
/// Standalone function (no MailClient needed) so it can be used with batch fetches.
pub fn process_raw_email(body: &[u8], is_unread: bool) -> Result<ProcessedEmail> {
    let raw_headers = extract_raw_headers(body);
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
    let date = parse_imap_date(&date_str).unwrap_or_else(|| {
        parse_date_from_received(&parsed.headers.get_all_values("Received"))
            .unwrap_or_else(Local::now)
    });

    let message_id = parsed
        .headers
        .get_first_value("Message-ID")
        .or_else(|| parsed.headers.get_first_value("Message-Id"))
        .unwrap_or_default()
        .trim()
        .to_string();
    let in_reply_to = parsed
        .headers
        .get_first_value("In-Reply-To")
        .unwrap_or_default()
        .trim()
        .to_string();
    let references_raw = parsed
        .headers
        .get_first_value("References")
        .unwrap_or_default();
    let references = parse_message_id_list(&references_raw).join(" ");

    let mut text_body = String::new();
    let mut html_body: Option<String> = None;
    let mut attachments = Vec::new();

    extract_parts(&parsed, &mut text_body, &mut html_body, &mut attachments);

    if text_body.is_empty()
        && let Some(html) = &html_body
    {
        text_body = html_to_text(html);
    }

    let preview: String = text_body
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect();

    let has_attachments = !attachments.is_empty();

    Ok(ProcessedEmail {
        from,
        to,
        subject,
        date,
        is_unread,
        preview,
        text_body,
        html_body,
        raw_headers,
        message_id,
        in_reply_to,
        references,
        has_attachments,
        attachments,
    })
}

fn extract_raw_headers(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    // Headers end at the first blank line (\r\n\r\n or \n\n)
    if let Some(pos) = text.find("\r\n\r\n") {
        text[..pos].to_string()
    } else if let Some(pos) = text.find("\n\n") {
        text[..pos].to_string()
    } else {
        text.to_string()
    }
}

fn extract_parts(
    part: &mailparse::ParsedMail,
    text_body: &mut String,
    html_body: &mut Option<String>,
    attachments: &mut Vec<AttachmentData>,
) {
    let content_type = part.ctype.mimetype.to_lowercase();
    let disposition = part.get_content_disposition();

    // Check if this part is an attachment:
    // 1. Explicit Content-Disposition: attachment
    // 2. Has a filename and is not a body type (text/plain, text/html), multipart, or inline image
    // 3. Leaf node with a non-body MIME type (catches PDFs, calendar invites, etc. without
    //    explicit disposition or filename)
    let is_inline_image = matches!(disposition.disposition, DispositionType::Inline)
        && content_type.starts_with("image/");
    let is_body_type = content_type == "text/plain" || content_type == "text/html";
    let is_leaf = part.subparts.is_empty();
    let is_crypto_sig = content_type == "application/pgp-signature"
        || content_type == "application/pkcs7-signature"
        || content_type == "application/x-pkcs7-signature";
    let is_attachment = matches!(disposition.disposition, DispositionType::Attachment)
        || (has_filename(&disposition, &part.ctype)
            && !is_body_type
            && !content_type.starts_with("multipart/")
            && !is_inline_image)
        || (is_leaf
            && !is_body_type
            && !content_type.starts_with("multipart/")
            && !is_inline_image
            && !is_crypto_sig);

    if is_attachment {
        let filename =
            get_attachment_filename(&disposition, &part.ctype, &part.headers, attachments.len());
        if let Ok(data) = part.get_body_raw() {
            let size_bytes = data.len();
            attachments.push(AttachmentData {
                filename,
                mime_type: content_type,
                size_bytes,
                data,
            });
        }
        return;
    }

    if content_type == "text/plain" {
        if let Ok(body) = part.get_body()
            && text_body.is_empty()
        {
            *text_body = body;
        }
    } else if content_type == "text/html"
        && let Ok(body) = part.get_body()
        && html_body.is_none()
    {
        *html_body = Some(body);
    }

    for sub in &part.subparts {
        extract_parts(sub, text_body, html_body, attachments);
    }
}

fn has_filename(
    disposition: &mailparse::ParsedContentDisposition,
    ctype: &mailparse::ParsedContentType,
) -> bool {
    disposition.params.contains_key("filename") || ctype.params.contains_key("name")
}

fn get_attachment_filename(
    disposition: &mailparse::ParsedContentDisposition,
    ctype: &mailparse::ParsedContentType,
    headers: &[mailparse::MailHeader],
    index: usize,
) -> String {
    // 1. Content-Disposition filename (decode RFC 2047 if present)
    if let Some(name) = disposition.params.get("filename")
        && !name.is_empty()
    {
        return decode_mime_str(name);
    }
    // 2. Content-Type name param
    if let Some(name) = ctype.params.get("name")
        && !name.is_empty()
    {
        return decode_mime_str(name);
    }
    // 3. Content-Description header (some mailers put the filename here)
    if let Some(desc) = headers.get_first_value("Content-Description")
        && !desc.trim().is_empty()
    {
        let decoded = decode_mime_str(desc.trim());
        if !decoded.is_empty() {
            return decoded;
        }
    }
    // 4. Fallback
    let ext = mime_to_extension(&ctype.mimetype);
    format!("attachment_{}.{}", index + 1, ext)
}

fn mime_to_extension(mime: &str) -> &str {
    match mime.to_lowercase().as_str() {
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "application/gzip" => "gz",
        "application/x-tar" => "tar",
        "application/x-bzip2" => "bz2",
        "application/x-7z-compressed" => "7z",
        "application/x-rar-compressed" => "rar",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.ms-powerpoint" => "ppt",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/json" => "json",
        "application/xml" | "text/xml" => "xml",
        "application/octet-stream" => "bin",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "audio/wav" => "wav",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "text/plain" => "txt",
        "text/html" => "html",
        "text/csv" => "csv",
        "text/calendar" => "ics",
        _ => "bin",
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

/// Replace non-RFC2822 timezone abbreviations with numeric UTC offsets.
/// mailparse handles the RFC2822 set (EST, EDT, CST, CDT, MST, MDT, PST, PDT,
/// UT, GMT) but many European and other mail servers use abbreviations like
/// CEST, CET, BST, JST, etc. that no standard email parser recognises.
fn normalize_timezone(s: &str) -> String {
    const TZ_MAP: &[(&str, &str)] = &[
        ("CEST", "+0200"),
        ("CET", "+0100"),
        ("EEST", "+0300"),
        ("EET", "+0200"),
        ("BST", "+0100"),
        ("WEST", "+0100"),
        ("WET", "+0000"),
        ("MEST", "+0200"),
        ("MET", "+0100"),
        ("IST", "+0530"),
        ("JST", "+0900"),
        ("KST", "+0900"),
        ("AEST", "+1000"),
        ("AEDT", "+1100"),
        ("ACST", "+0930"),
        ("ACDT", "+1030"),
        ("NZST", "+1200"),
        ("NZDT", "+1300"),
        ("MSK", "+0300"),
        ("SAST", "+0200"),
        ("HKT", "+0800"),
        ("SGT", "+0800"),
        ("ICT", "+0700"),
        ("PKT", "+0500"),
        ("BRT", "-0300"),
        ("ART", "-0300"),
        ("AKST", "-0900"),
        ("AKDT", "-0800"),
        ("HST", "-1000"),
        ("NST", "-0330"),
        ("NDT", "-0230"),
        ("ADT", "-0300"),
        ("AST", "-0400"),
    ];

    let trimmed = s.trim();
    if let Some(pos) = trimmed.rfind(char::is_whitespace) {
        let tz_part = &trimmed[pos + 1..];
        for (name, offset) in TZ_MAP {
            if tz_part.eq_ignore_ascii_case(name) {
                return format!("{}{}", &trimmed[..=pos], offset);
            }
        }
    }

    s.to_string()
}

fn parse_imap_date(date_str: &str) -> Option<DateTime<Local>> {
    // Normalize whitespace (some old servers produce double-spaces)
    let normalized: String = date_str.split_whitespace().collect::<Vec<_>>().join(" ");
    // Replace non-standard timezone abbreviations (CEST, BST, JST, etc.)
    let normalized = normalize_timezone(&normalized);
    let s = normalized.trim();

    if s.is_empty() {
        return None;
    }

    // Use mailparse::dateparse first — handles named timezones (EST, PST, etc.),
    // parenthesized comments, and many non-standard formats from old mail servers
    if let Ok(ts) = mailparse::dateparse(s)
        && let Some(dt) = DateTime::from_timestamp(ts, 0)
    {
        return Some(dt.with_timezone(&Local));
    }

    // Fallback: try chrono's RFC2822 parser
    if let Ok(dt) = DateTime::parse_from_rfc2822(s) {
        return Some(dt.with_timezone(&Local));
    }

    // Last resort: try common formats without timezone, assume UTC
    let formats = ["%a, %d %b %Y %H:%M:%S", "%d %b %Y %H:%M:%S"];
    for fmt in &formats {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt.and_utc().with_timezone(&Local));
        }
    }

    None
}

/// Try to extract a date from Received headers. Each header typically looks like:
/// `from ... by ... ; Wed, 26 Nov 2008 03:32:27 -0800 (PST)`
/// The date part comes after the semicolon.
fn parse_date_from_received(received_headers: &[String]) -> Option<DateTime<Local>> {
    for header in received_headers {
        // Split on ';' and try to parse each part after a semicolon
        for part in header.split(';').skip(1) {
            if let Some(dt) = parse_imap_date(part.trim()) {
                return Some(dt);
            }
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

    if seconds < 5 {
        "just now".to_string()
    } else if seconds < 60 {
        format!("{}s ago", seconds)
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

/// Extract all `<...>` message-id tokens from a header value.
pub fn parse_message_id_list(header: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = header;
    while let Some(start) = rest.find('<') {
        if let Some(end) = rest[start..].find('>') {
            ids.push(rest[start..start + end + 1].to_string());
            rest = &rest[start + end + 1..];
        } else {
            break;
        }
    }
    ids
}

/// Strip Re:/Fwd:/Fw: prefixes (case-insensitive, repeating) for thread grouping.
pub fn normalize_subject(subject: &str) -> String {
    let mut s = subject.trim();
    loop {
        let lower = s.to_lowercase();
        if lower.starts_with("re:") {
            s = s[3..].trim_start();
        } else if lower.starts_with("fwd:") {
            s = s[4..].trim_start();
        } else if lower.starts_with("fw:") {
            s = s[3..].trim_start();
        } else if lower.starts_with("re[") {
            // Handle Re[2]: etc.
            if let Some(close) = s.find("]:") {
                s = s[close + 2..].trim_start();
            } else {
                break;
            }
        } else {
            break;
        }
    }
    s.to_string()
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
