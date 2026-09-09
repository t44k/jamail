use crate::config::SmtpConfig;
use anyhow::{Context, Result};
use lettre::message::{Mailbox, MultiPart, SinglePart, header::ContentType};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

pub struct ComposeAttachment {
    pub path: std::path::PathBuf,
    pub filename: String,
    pub size: u64,
}

/// A single queued outgoing email.
pub struct SendJob {
    /// Which configured account this send belongs to — captured at enqueue
    /// time (not re-read from "the current account" later) so a
    /// Sent-folder upload always targets the account the message was
    /// actually sent from, even if the user has since switched accounts in
    /// the UI while this job was in flight on the background send thread.
    pub account: String,
    pub smtp_config: SmtpConfig,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub attachments: Vec<ComposeAttachment>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
    /// Draft ID to mark as sent on success.
    pub draft_id: Option<i64>,
    /// Remote IMAP folder to upload a copy of the sent message to, if the
    /// account is configured for it (`accounts.<name>.sent_folder`).
    pub sent_folder: Option<String>,
}

/// Result returned from the send worker for each completed job.
pub struct SendResult {
    /// Echoed back from the job (see `SendJob::account`).
    pub account: String,
    pub draft_id: Option<i64>,
    pub error: Option<String>,
    /// Echoed back from the job so the caller can enqueue a Sent-folder
    /// upload; `None` on failure even if the job requested one.
    pub sent_folder: Option<String>,
    /// The raw RFC-2822 bytes of the message that was actually sent, used
    /// as the APPEND payload for the Sent-folder upload.
    pub raw_message: Option<Vec<u8>>,
}

/// Shared send-queue counters visible to the UI.
pub struct SendCounters {
    /// Number of messages currently in-flight or waiting.
    pub pending: AtomicUsize,
    /// Cumulative send errors since startup (never reset automatically).
    pub errors: AtomicUsize,
}

impl SendCounters {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: AtomicUsize::new(0),
            errors: AtomicUsize::new(0),
        })
    }
}

/// A handle for enqueuing outgoing emails.
///
/// Spawns one background worker thread that processes jobs serially.
/// Completed results are delivered via `result_rx`.
pub struct SendQueue {
    pub job_tx: mpsc::Sender<SendJob>,
    pub result_rx: mpsc::Receiver<SendResult>,
    pub counters: Arc<SendCounters>,
}

impl Default for SendQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl SendQueue {
    pub fn new() -> Self {
        let (job_tx, job_rx) = mpsc::channel::<SendJob>();
        let (result_tx, result_rx) = mpsc::channel::<SendResult>();
        let counters = SendCounters::new();
        let counters_bg = Arc::clone(&counters);

        std::thread::spawn(move || {
            while let Ok(job) = job_rx.recv() {
                let result = send_email(
                    &job.smtp_config,
                    &job.from,
                    &job.to,
                    &job.cc,
                    &job.bcc,
                    &job.subject,
                    &job.body,
                    &job.attachments,
                    job.in_reply_to.as_deref(),
                    job.references.as_deref(),
                );
                // Decrement pending regardless of outcome.
                counters_bg.pending.fetch_sub(1, Ordering::Relaxed);
                let (error, raw_message, sent_folder) = match result {
                    Ok(raw) => (None, Some(raw), job.sent_folder),
                    Err(e) => (Some(e.to_string()), None, None),
                };
                if error.is_some() {
                    counters_bg.errors.fetch_add(1, Ordering::Relaxed);
                }
                let _ = result_tx.send(SendResult {
                    account: job.account,
                    draft_id: job.draft_id,
                    error,
                    sent_folder,
                    raw_message,
                });
            }
        });

        Self {
            job_tx,
            result_rx,
            counters,
        }
    }

    /// Enqueue a send job. Increments `pending` before returning.
    pub fn enqueue(&self, job: SendJob) {
        self.counters.pending.fetch_add(1, Ordering::Relaxed);
        let _ = self.job_tx.send(job);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn send_email(
    smtp_config: &SmtpConfig,
    from: &str,
    to: &str,
    cc: &str,
    bcc: &str,
    subject: &str,
    body: &str,
    attachments: &[ComposeAttachment],
    in_reply_to: Option<&str>,
    references: Option<&str>,
) -> Result<Vec<u8>> {
    let from_mailbox: Mailbox = from
        .parse()
        .with_context(|| format!("Invalid From address: {}", from))?;

    let mut builder = Message::builder().from(from_mailbox).subject(subject);

    for addr in split_addresses(to) {
        let mailbox: Mailbox = addr
            .parse()
            .with_context(|| format!("Invalid To address: {}", addr))?;
        builder = builder.to(mailbox);
    }

    for addr in split_addresses(cc) {
        let mailbox: Mailbox = addr
            .parse()
            .with_context(|| format!("Invalid Cc address: {}", addr))?;
        builder = builder.cc(mailbox);
    }

    for addr in split_addresses(bcc) {
        let mailbox: Mailbox = addr
            .parse()
            .with_context(|| format!("Invalid Bcc address: {}", addr))?;
        builder = builder.bcc(mailbox);
    }

    if let Some(irt) = in_reply_to {
        builder = builder.in_reply_to(irt.to_string());
    }

    if let Some(refs) = references {
        builder = builder.references(refs.to_string());
    }

    let message = if attachments.is_empty() {
        builder
            .body(body.to_string())
            .context("Failed to build email message")?
    } else {
        let mut multipart = MultiPart::mixed().singlepart(
            SinglePart::builder()
                .content_type(ContentType::TEXT_PLAIN)
                .body(body.to_string()),
        );

        for att in attachments {
            let data = std::fs::read(&att.path)
                .with_context(|| format!("Failed to read attachment: {}", att.path.display()))?;

            let mime_str = mime_guess::from_path(&att.filename)
                .first_or_octet_stream()
                .to_string();
            let content_type = ContentType::parse(&mime_str).unwrap_or(ContentType::TEXT_PLAIN);

            let attachment_part =
                lettre::message::Attachment::new(att.filename.clone()).body(data, content_type);
            multipart = multipart.singlepart(attachment_part);
        }

        builder
            .multipart(multipart)
            .context("Failed to build multipart email")?
    };

    let password = smtp_config.auth.resolve_password()?;
    let creds = Credentials::new(smtp_config.login.clone(), password);

    let transport = if smtp_config.starttls {
        SmtpTransport::starttls_relay(&smtp_config.host)
            .context("Failed to create STARTTLS transport")?
            .port(smtp_config.port)
            .credentials(creds)
            .build()
    } else {
        SmtpTransport::relay(&smtp_config.host)
            .context("Failed to create TLS transport")?
            .port(smtp_config.port)
            .credentials(creds)
            .build()
    };

    transport
        .send(&message)
        .context("Failed to send email via SMTP")?;

    Ok(message.formatted())
}

/// Build a raw RFC-2822-shaped message for uploading a draft to a remote
/// Drafts folder. Unlike `send_email`, this tolerates empty To/Cc/Bcc
/// (a draft in progress may not have recipients yet) since it never goes
/// through an SMTP envelope — it's purely a client-side courtesy copy.
pub fn build_draft_raw(
    from: &str,
    to: &str,
    cc: &str,
    bcc: &str,
    subject: &str,
    body: &str,
) -> Vec<u8> {
    let mut out = String::new();
    out.push_str(&format!("From: {}\r\n", from));
    if !to.trim().is_empty() {
        out.push_str(&format!("To: {}\r\n", to));
    }
    if !cc.trim().is_empty() {
        out.push_str(&format!("Cc: {}\r\n", cc));
    }
    if !bcc.trim().is_empty() {
        out.push_str(&format!("Bcc: {}\r\n", bcc));
    }
    out.push_str(&format!("Subject: {}\r\n", subject));
    out.push_str("MIME-Version: 1.0\r\n");
    out.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    out.push_str("\r\n");
    out.push_str(&body.replace("\r\n", "\n").replace('\n', "\r\n"));
    out.into_bytes()
}

fn split_addresses(s: &str) -> Vec<String> {
    s.split(',')
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .collect()
}

pub fn format_attachment_size(size: u64) -> String {
    if size < 1024 {
        format!("{} B", size)
    } else if size < 1024 * 1024 {
        format!("{:.1} KB", size as f64 / 1024.0)
    } else {
        format!("{:.1} MB", size as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_draft_raw_omits_empty_recipient_headers() {
        let raw = build_draft_raw("a@b.com", "", "", "", "Hi", "body text");
        let text = String::from_utf8(raw).unwrap();
        assert!(text.starts_with("From: a@b.com\r\n"));
        assert!(!text.contains("To:"));
        assert!(!text.contains("Cc:"));
        assert!(!text.contains("Bcc:"));
        assert!(text.contains("Subject: Hi\r\n"));
        assert!(text.ends_with("body text"));
    }

    #[test]
    fn build_draft_raw_includes_recipients_when_present() {
        let raw = build_draft_raw(
            "a@b.com",
            "c@d.com",
            "e@f.com",
            "g@h.com",
            "Subj",
            "line1\nline2",
        );
        let text = String::from_utf8(raw).unwrap();
        assert!(text.contains("To: c@d.com\r\n"));
        assert!(text.contains("Cc: e@f.com\r\n"));
        assert!(text.contains("Bcc: g@h.com\r\n"));
        assert!(text.contains("line1\r\nline2"));
    }

    #[test]
    fn split_addresses_trims_and_drops_empty_entries() {
        assert_eq!(
            split_addresses(" a@b.com ,  , c@d.com,"),
            vec!["a@b.com".to_string(), "c@d.com".to_string()]
        );
    }

    #[test]
    fn format_attachment_size_picks_appropriate_unit() {
        assert_eq!(format_attachment_size(500), "500 B");
        assert_eq!(format_attachment_size(2048), "2.0 KB");
        assert_eq!(format_attachment_size(5 * 1024 * 1024), "5.0 MB");
    }
}
