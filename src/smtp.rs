use crate::config::SmtpConfig;
use anyhow::{Context, Result};
use lettre::message::{header::ContentType, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

pub struct ComposeAttachment {
    pub path: std::path::PathBuf,
    pub filename: String,
    pub size: u64,
}

/// A single queued outgoing email.
pub struct SendJob {
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
}

/// Result returned from the send worker for each completed job.
pub struct SendResult {
    pub draft_id: Option<i64>,
    pub error: Option<String>,
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
                let error = result.err().map(|e| e.to_string());
                if error.is_some() {
                    counters_bg.errors.fetch_add(1, Ordering::Relaxed);
                }
                let _ = result_tx.send(SendResult {
                    draft_id: job.draft_id,
                    error,
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
) -> Result<()> {
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

    Ok(())
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
