use crate::config::SmtpConfig;
use anyhow::{Context, Result};
use lettre::message::{Mailbox, MultiPart, SinglePart, header::ContentType};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};

pub struct ComposeAttachment {
    pub path: std::path::PathBuf,
    pub filename: String,
    pub size: u64,
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
