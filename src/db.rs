use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::io::Read;
use std::path::PathBuf;

use crate::mail::{AttachmentData, Email, EmailContent, FolderInfo};
use chrono::{DateTime, Local, TimeZone};

const CURRENT_SCHEMA_VERSION: i32 = 4;

pub struct AttachmentMeta {
    pub id: i64,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: usize,
}

pub struct MailDb {
    conn: Connection,
}

fn db_path() -> Result<PathBuf> {
    let data_dir = dirs::data_dir().context("Could not determine data directory")?;
    let dir = data_dir.join("jamail");
    std::fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    Ok(dir.join("mail.db"))
}

fn compress(data: &[u8]) -> Vec<u8> {
    zstd::encode_all(data, 3).unwrap_or_else(|_| data.to_vec())
}

fn decompress(data: &[u8]) -> Vec<u8> {
    let mut decoder = zstd::Decoder::new(data).unwrap();
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).unwrap_or_default();
    out
}

fn get_schema_version(conn: &Connection) -> i32 {
    // Check if schema_version table exists
    let has_table: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |row| row.get::<_, i32>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);

    if !has_table {
        return 0;
    }

    conn.query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .unwrap_or(0)
}

fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS emails_fts;
         DROP TABLE IF EXISTS attachments;
         DROP TABLE IF EXISTS emails;
         DROP TABLE IF EXISTS sync_state;
         DROP TABLE IF EXISTS folders;
         DROP TABLE IF EXISTS local_messages;
         DROP TABLE IF EXISTS schema_version;

         CREATE TABLE schema_version (version INTEGER NOT NULL);
         INSERT INTO schema_version VALUES (4);

         CREATE TABLE folders (
             account   TEXT NOT NULL,
             name      TEXT NOT NULL,
             delimiter TEXT NOT NULL DEFAULT '/',
             updated   TEXT,
             PRIMARY KEY (account, name)
         );

         CREATE TABLE emails (
             id              INTEGER PRIMARY KEY AUTOINCREMENT,
             account         TEXT NOT NULL,
             folder          TEXT NOT NULL,
             uid             INTEGER NOT NULL,
             from_addr       TEXT NOT NULL,
             to_addr         TEXT NOT NULL DEFAULT '',
             subject         TEXT NOT NULL,
             date_ts         INTEGER NOT NULL,
             date_rfc3339    TEXT NOT NULL,
             is_unread       INTEGER NOT NULL DEFAULT 1,
             preview         TEXT NOT NULL DEFAULT '',
             text_body_zstd  BLOB,
             html_body_zstd  BLOB,
             message_id      TEXT NOT NULL DEFAULT '',
             in_reply_to     TEXT NOT NULL DEFAULT '',
             refs            TEXT NOT NULL DEFAULT '',
             has_attachments INTEGER NOT NULL DEFAULT 0,
             raw_headers_zstd BLOB,
             UNIQUE(account, folder, uid)
         );

         CREATE INDEX idx_emails_acct_folder_date ON emails(account, folder, date_ts DESC);
         CREATE INDEX idx_emails_message_id ON emails(message_id);

         CREATE TABLE sync_state (
             account     TEXT NOT NULL,
             folder      TEXT NOT NULL,
             uidvalidity INTEGER NOT NULL DEFAULT 0,
             last_uid    INTEGER NOT NULL DEFAULT 0,
             last_sync   TEXT,
             PRIMARY KEY (account, folder)
         );

         CREATE VIRTUAL TABLE emails_fts USING fts5(
             from_addr, subject, body_text,
             tokenize='unicode61 remove_diacritics 2'
         );

         CREATE TABLE attachments (
             id           INTEGER PRIMARY KEY AUTOINCREMENT,
             email_id     INTEGER NOT NULL,
             filename     TEXT NOT NULL,
             mime_type    TEXT NOT NULL DEFAULT '',
             size_bytes   INTEGER NOT NULL DEFAULT 0,
             content_zstd BLOB NOT NULL
         );
         CREATE INDEX idx_attachments_email_id ON attachments(email_id);

         CREATE TABLE local_messages (
             id              INTEGER PRIMARY KEY AUTOINCREMENT,
             account         TEXT NOT NULL,
             status          TEXT NOT NULL DEFAULT 'draft',
             from_addr       TEXT NOT NULL DEFAULT '',
             to_addr         TEXT NOT NULL DEFAULT '',
             cc              TEXT NOT NULL DEFAULT '',
             bcc             TEXT NOT NULL DEFAULT '',
             subject         TEXT NOT NULL DEFAULT '',
             body            TEXT NOT NULL DEFAULT '',
             in_reply_to     TEXT NOT NULL DEFAULT '',
             refs            TEXT NOT NULL DEFAULT '',
             created_at      TEXT NOT NULL,
             sent_at         TEXT,
             error           TEXT,
             upload_status   TEXT NOT NULL DEFAULT 'none',
             upload_error    TEXT
         );
         CREATE INDEX idx_local_messages_status ON local_messages(status, created_at DESC);",
    )?;
    Ok(())
}

/// Status values for `local_messages.status`.
pub const STATUS_DRAFT: &str = "draft";
pub const STATUS_SENDING: &str = "sending";
pub const STATUS_SEND_ERROR: &str = "send_error";
pub const STATUS_SENT: &str = "sent";

/// Status values for `local_messages.upload_status`.
pub const UPLOAD_NONE: &str = "none";
pub const UPLOAD_PENDING: &str = "pending";
pub const UPLOAD_UPLOADED: &str = "uploaded";
pub const UPLOAD_ERROR: &str = "error";

impl MailDb {
    pub fn open() -> Result<Self> {
        let path = db_path()?;
        let conn = Connection::open(&path)
            .with_context(|| format!("Failed to open database at {}", path.display()))?;

        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA cache_size = -4000;
             PRAGMA mmap_size = 0;",
        )?;

        let version = get_schema_version(&conn);
        if version < CURRENT_SCHEMA_VERSION {
            create_schema(&conn)?;
        }

        Ok(Self { conn })
    }

    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("Failed to open in-memory database")?;
        create_schema(&conn)?;
        Ok(Self { conn })
    }

    pub fn begin_tx(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        Ok(())
    }

    pub fn commit_tx(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    pub fn rollback_tx(&self) {
        let _ = self.conn.execute_batch("ROLLBACK");
    }

    /// Insert a single email inside an already-open transaction.
    /// Caller is responsible for BEGIN/COMMIT/ROLLBACK.
    #[allow(clippy::too_many_arguments)]
    pub fn store_email_core(
        &self,
        account: &str,
        folder: &str,
        uid: u32,
        from: &str,
        to: &str,
        subject: &str,
        date: &DateTime<Local>,
        is_unread: bool,
        preview: &str,
        text_body: &str,
        html_body: Option<&str>,
        raw_headers: &str,
        message_id: &str,
        in_reply_to: &str,
        refs: &str,
        has_attachments: bool,
        attachments: &[AttachmentData],
    ) -> Result<i64> {
        let date_ts = date.timestamp();
        let date_rfc3339 = date.to_rfc3339();
        let text_compressed = if text_body.is_empty() {
            None
        } else {
            Some(compress(text_body.as_bytes()))
        };
        let html_compressed = html_body.map(|h| compress(h.as_bytes()));
        let headers_compressed = if raw_headers.is_empty() {
            None
        } else {
            Some(compress(raw_headers.as_bytes()))
        };

        // Delete old entry if exists (for REPLACE semantics)
        let old_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM emails WHERE account = ?1 AND folder = ?2 AND uid = ?3",
                params![account, folder, uid],
                |row| row.get(0),
            )
            .ok();

        if let Some(old_id) = old_id {
            self.conn
                .execute("DELETE FROM emails_fts WHERE rowid = ?1", params![old_id])?;
            self.conn.execute(
                "DELETE FROM attachments WHERE email_id = ?1",
                params![old_id],
            )?;
            self.conn
                .execute("DELETE FROM emails WHERE id = ?1", params![old_id])?;
        }

        self.conn.execute(
            "INSERT INTO emails
                (account, folder, uid, from_addr, to_addr, subject, date_ts, date_rfc3339,
                 is_unread, preview, text_body_zstd, html_body_zstd, raw_headers_zstd,
                 message_id, in_reply_to, refs, has_attachments)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                account,
                folder,
                uid,
                from,
                to,
                subject,
                date_ts,
                date_rfc3339,
                is_unread as i32,
                preview,
                text_compressed,
                html_compressed,
                headers_compressed,
                message_id,
                in_reply_to,
                refs,
                has_attachments as i32,
            ],
        )?;

        let email_id = self.conn.last_insert_rowid();

        // Update FTS
        self.conn.execute(
            "INSERT INTO emails_fts (rowid, from_addr, subject, body_text) VALUES (?1, ?2, ?3, ?4)",
            params![email_id, from, subject, text_body],
        )?;

        // Store attachments
        for att in attachments {
            let compressed = compress(&att.data);
            self.conn.execute(
                "INSERT INTO attachments (email_id, filename, mime_type, size_bytes, content_zstd)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    email_id,
                    att.filename,
                    att.mime_type,
                    att.size_bytes as i64,
                    compressed
                ],
            )?;
        }

        Ok(email_id)
    }

    /// Store a single email, wrapped in its own transaction.
    #[allow(clippy::too_many_arguments, dead_code)]
    pub fn store_email(
        &self,
        account: &str,
        folder: &str,
        uid: u32,
        from: &str,
        to: &str,
        subject: &str,
        date: &DateTime<Local>,
        is_unread: bool,
        preview: &str,
        text_body: &str,
        html_body: Option<&str>,
        raw_headers: &str,
        message_id: &str,
        in_reply_to: &str,
        refs: &str,
        has_attachments: bool,
        attachments: &[AttachmentData],
    ) -> Result<i64> {
        self.begin_tx()?;
        let result = self.store_email_core(
            account,
            folder,
            uid,
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
            refs,
            has_attachments,
            attachments,
        );
        match result {
            Ok(id) => {
                self.commit_tx()?;
                Ok(id)
            }
            Err(e) => {
                self.rollback_tx();
                Err(e)
            }
        }
    }

    pub fn get_email_list(&self, account: &str, folder: &str) -> Result<Vec<Email>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, uid, from_addr, subject, date_ts, date_rfc3339, is_unread, preview,
                    message_id, in_reply_to, refs, has_attachments, account
             FROM emails WHERE account = ?1 AND folder = ?2
             ORDER BY date_ts DESC",
        )?;

        let rows = stmt.query_map(params![account, folder], |row| {
            let id: i64 = row.get(0)?;
            let uid: u32 = row.get(1)?;
            let from: String = row.get(2)?;
            let subject: String = row.get(3)?;
            let date_ts: i64 = row.get(4)?;
            let _date_rfc3339: String = row.get(5)?;
            let is_unread: bool = row.get::<_, i32>(6)? != 0;
            let preview: String = row.get(7)?;
            let message_id: String = row.get(8)?;
            let in_reply_to: String = row.get(9)?;
            let references: String = row.get(10)?;
            let has_attachments: bool = row.get::<_, i32>(11)? != 0;
            let acct: String = row.get(12)?;

            let date = Local
                .timestamp_opt(date_ts, 0)
                .single()
                .unwrap_or_else(Local::now);

            Ok(Email {
                id,
                uid,
                account: acct,
                from,
                subject,
                date,
                is_unread,
                preview,
                message_id,
                in_reply_to,
                references,
                has_attachments,
                status_label: None,
            })
        })?;

        let mut emails = Vec::new();
        for row in rows {
            emails.push(row?);
        }
        Ok(emails)
    }

    pub fn get_email_content(&self, id: i64) -> Result<Option<EmailContent>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_addr, to_addr, subject, date_ts, text_body_zstd, html_body_zstd,
                    raw_headers_zstd
             FROM emails WHERE id = ?1",
        )?;

        let mut rows = stmt.query(params![id])?;
        let row = match rows.next()? {
            Some(r) => r,
            None => return Ok(None),
        };

        let from: String = row.get(0)?;
        let to: String = row.get(1)?;
        let subject: String = row.get(2)?;
        let date_ts: i64 = row.get(3)?;
        let text_zstd: Option<Vec<u8>> = row.get(4)?;
        let html_zstd: Option<Vec<u8>> = row.get(5)?;
        let headers_zstd: Option<Vec<u8>> = row.get(6)?;

        let date = Local
            .timestamp_opt(date_ts, 0)
            .single()
            .unwrap_or_else(Local::now);

        let text_body = text_zstd
            .map(|z| String::from_utf8_lossy(&decompress(&z)).to_string())
            .unwrap_or_default();

        let html_body = html_zstd.map(|z| String::from_utf8_lossy(&decompress(&z)).to_string());

        let raw_headers = headers_zstd
            .map(|z| String::from_utf8_lossy(&decompress(&z)).to_string())
            .unwrap_or_default();

        Ok(Some(EmailContent {
            from,
            to,
            subject,
            date,
            text_body,
            html_body,
            raw_headers,
        }))
    }

    pub fn get_global_inbox(&self) -> Result<Vec<Email>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, uid, from_addr, subject, date_ts, date_rfc3339, is_unread, preview,
                    message_id, in_reply_to, refs, has_attachments, account
             FROM emails WHERE folder = 'INBOX'
             ORDER BY date_ts DESC",
        )?;

        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let uid: u32 = row.get(1)?;
            let from: String = row.get(2)?;
            let subject: String = row.get(3)?;
            let date_ts: i64 = row.get(4)?;
            let _date_rfc3339: String = row.get(5)?;
            let is_unread: bool = row.get::<_, i32>(6)? != 0;
            let preview: String = row.get(7)?;
            let message_id: String = row.get(8)?;
            let in_reply_to: String = row.get(9)?;
            let references: String = row.get(10)?;
            let has_attachments: bool = row.get::<_, i32>(11)? != 0;
            let acct: String = row.get(12)?;

            let date = Local
                .timestamp_opt(date_ts, 0)
                .single()
                .unwrap_or_else(Local::now);

            Ok(Email {
                id,
                uid,
                account: acct,
                from,
                subject,
                date,
                is_unread,
                preview,
                message_id,
                in_reply_to,
                references,
                has_attachments,
                status_label: None,
            })
        })?;

        let mut emails = Vec::new();
        for row in rows {
            emails.push(row?);
        }
        Ok(emails)
    }

    pub fn mark_read(&self, id: i64) -> Result<()> {
        self.conn
            .execute("UPDATE emails SET is_unread = 0 WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn search_emails(&self, account: &str, folder: &str, query: &str) -> Result<Vec<Email>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.id, e.uid, e.from_addr, e.subject, e.date_ts, e.is_unread, e.preview,
                    e.message_id, e.in_reply_to, e.refs, e.has_attachments, e.account
             FROM emails_fts f
             JOIN emails e ON e.id = f.rowid
             WHERE emails_fts MATCH ?1
               AND e.account = ?2 AND e.folder = ?3
             ORDER BY e.date_ts DESC",
        )?;

        let rows = stmt.query_map(params![query, account, folder], |row| {
            let id: i64 = row.get(0)?;
            let uid: u32 = row.get(1)?;
            let from: String = row.get(2)?;
            let subject: String = row.get(3)?;
            let date_ts: i64 = row.get(4)?;
            let is_unread: bool = row.get::<_, i32>(5)? != 0;
            let preview: String = row.get(6)?;
            let message_id: String = row.get(7)?;
            let in_reply_to: String = row.get(8)?;
            let references: String = row.get(9)?;
            let has_attachments: bool = row.get::<_, i32>(10)? != 0;
            let acct: String = row.get(11)?;

            let date = Local
                .timestamp_opt(date_ts, 0)
                .single()
                .unwrap_or_else(Local::now);

            Ok(Email {
                id,
                uid,
                account: acct,
                from,
                subject,
                date,
                is_unread,
                preview,
                message_id,
                in_reply_to,
                references,
                has_attachments,
                status_label: None,
            })
        })?;

        let mut emails = Vec::new();
        for row in rows {
            emails.push(row?);
        }
        Ok(emails)
    }

    pub fn has_email(&self, account: &str, folder: &str, uid: u32) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM emails WHERE account = ?1 AND folder = ?2 AND uid = ?3",
                params![account, folder, uid],
                |_| Ok(()),
            )
            .is_ok()
    }

    pub fn get_sync_state(&self, account: &str, folder: &str) -> Result<Option<(u32, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT uidvalidity, last_uid FROM sync_state WHERE account = ?1 AND folder = ?2",
        )?;
        let mut rows = stmt.query(params![account, folder])?;
        match rows.next()? {
            Some(row) => {
                let uidvalidity: u32 = row.get(0)?;
                let last_uid: u32 = row.get(1)?;
                Ok(Some((uidvalidity, last_uid)))
            }
            None => Ok(None),
        }
    }

    pub fn set_sync_state(
        &self,
        account: &str,
        folder: &str,
        uidvalidity: u32,
        last_uid: u32,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sync_state (account, folder, uidvalidity, last_uid, last_sync)
             VALUES (?1, ?2, ?3, ?4, datetime('now'))",
            params![account, folder, uidvalidity, last_uid],
        )?;
        Ok(())
    }

    pub fn get_attachments_meta(&self, email_id: i64) -> Result<Vec<AttachmentMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, filename, mime_type, size_bytes
             FROM attachments WHERE email_id = ?1
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![email_id], |row| {
            Ok(AttachmentMeta {
                id: row.get(0)?,
                filename: row.get(1)?,
                mime_type: row.get(2)?,
                size_bytes: row.get::<_, i64>(3)? as usize,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn get_attachment_data(&self, id: i64) -> Result<Option<Vec<u8>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT content_zstd FROM attachments WHERE id = ?1")?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => {
                let compressed: Vec<u8> = row.get(0)?;
                Ok(Some(decompress(&compressed)))
            }
            None => Ok(None),
        }
    }

    pub fn clear_folder_emails(&self, account: &str, folder: &str) -> Result<()> {
        // Delete FTS entries and attachments for emails in this folder
        self.conn.execute(
            "DELETE FROM emails_fts WHERE rowid IN (SELECT id FROM emails WHERE account = ?1 AND folder = ?2)",
            params![account, folder],
        )?;
        self.conn.execute(
            "DELETE FROM attachments WHERE email_id IN (SELECT id FROM emails WHERE account = ?1 AND folder = ?2)",
            params![account, folder],
        )?;
        self.conn.execute(
            "DELETE FROM emails WHERE account = ?1 AND folder = ?2",
            params![account, folder],
        )?;
        self.conn.execute(
            "DELETE FROM sync_state WHERE account = ?1 AND folder = ?2",
            params![account, folder],
        )?;
        Ok(())
    }

    pub fn store_folders(&self, account: &str, folders: &[FolderInfo]) -> Result<()> {
        self.conn
            .execute("DELETE FROM folders WHERE account = ?1", params![account])?;
        for f in folders {
            self.conn.execute(
                "INSERT INTO folders (account, name, delimiter, updated)
                 VALUES (?1, ?2, ?3, datetime('now'))",
                params![account, f.name, f.delimiter],
            )?;
        }
        Ok(())
    }

    pub fn get_known_addresses(&self) -> Result<Vec<String>> {
        let mut addrs = std::collections::HashSet::new();

        let mut stmt = self.conn.prepare("SELECT DISTINCT from_addr FROM emails")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for addr in rows.flatten() {
            let trimmed = addr.trim().to_string();
            if !trimmed.is_empty() {
                addrs.insert(trimmed);
            }
        }

        let mut stmt = self.conn.prepare("SELECT DISTINCT to_addr FROM emails")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for to_field in rows.flatten() {
            for part in to_field.split(',') {
                let trimmed = part.trim().to_string();
                if !trimmed.is_empty() {
                    addrs.insert(trimmed);
                }
            }
        }

        let mut result: Vec<String> = addrs.into_iter().collect();
        result.sort_by_key(|a| a.to_lowercase());
        Ok(result)
    }

    pub fn get_all_uids(&self, account: &str, folder: &str) -> Result<Vec<u32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT uid FROM emails WHERE account = ?1 AND folder = ?2")?;
        let rows = stmt.query_map(params![account, folder], |row| row.get(0))?;
        let mut uids = Vec::new();
        for row in rows {
            uids.push(row?);
        }
        Ok(uids)
    }

    pub fn update_flags(&self, account: &str, folder: &str, flags: &[(u32, bool)]) -> Result<bool> {
        let mut changed = false;
        for &(uid, is_unread) in flags {
            let rows = self.conn.execute(
                "UPDATE emails SET is_unread = ?1 WHERE account = ?2 AND folder = ?3 AND uid = ?4 AND is_unread != ?1",
                params![is_unread as i32, account, folder, uid],
            )?;
            if rows > 0 {
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Look up the (account, uid, folder) an email belongs to by its local
    /// `id`. Includes `account` (not just `uid`/`folder`) because `id` is a
    /// single autoincrement PK shared across every account (see CLAUDE.md
    /// "Emails have an autoincrement id... All UI lookups use id") — with
    /// `jamaild` now syncing every configured account concurrently, callers
    /// that route a request to a specific account's IMAP connection (e.g.
    /// mark-seen) need to know *which* account, not just assume "the
    /// current one" (wrong for the cross-account Global Inbox view).
    pub fn get_email_account_uid_and_folder(
        &self,
        id: i64,
    ) -> Result<Option<(String, u32, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT account, uid, folder FROM emails WHERE id = ?1")?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => {
                let account: String = row.get(0)?;
                let uid: u32 = row.get(1)?;
                let folder: String = row.get(2)?;
                Ok(Some((account, uid, folder)))
            }
            None => Ok(None),
        }
    }

    pub fn get_folders(&self, account: &str) -> Result<Vec<FolderInfo>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, delimiter FROM folders WHERE account = ?1 ORDER BY name")?;
        let rows = stmt.query_map(params![account], |row| {
            Ok(FolderInfo {
                name: row.get(0)?,
                delimiter: row.get(1)?,
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    // ── Local messages (drafts/sent) ──────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub fn save_draft(
        &self,
        account: &str,
        from: &str,
        to: &str,
        cc: &str,
        bcc: &str,
        subject: &str,
        body: &str,
        in_reply_to: &str,
        refs: &str,
    ) -> Result<i64> {
        let now = Local::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO local_messages (account, status, from_addr, to_addr, cc, bcc, subject, body, in_reply_to, refs, created_at)
             VALUES (?1, 'draft', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![account, from, to, cc, bcc, subject, body, in_reply_to, refs, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_draft(
        &self,
        id: i64,
        from: &str,
        to: &str,
        cc: &str,
        bcc: &str,
        subject: &str,
        body: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE local_messages SET from_addr=?2, to_addr=?3, cc=?4, bcc=?5, subject=?6, body=?7
             WHERE id=?1 AND status='draft'",
            params![id, from, to, cc, bcc, subject, body],
        )?;
        Ok(())
    }

    /// Mark a draft as actively being sent (in-flight). Keeps it visible in
    /// the Drafts list — with a "Sending…" indicator — instead of it
    /// silently disappearing while the background send worker runs.
    pub fn mark_draft_sending(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE local_messages SET status=?2 WHERE id=?1",
            params![id, STATUS_SENDING],
        )?;
        Ok(())
    }

    /// Record a failed send attempt. The message stays in the Drafts list
    /// (not "sent") with the error text attached so it doesn't vanish.
    pub fn mark_draft_send_error(&self, id: i64, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE local_messages SET status=?2, error=?3 WHERE id=?1",
            params![id, STATUS_SEND_ERROR, error],
        )?;
        Ok(())
    }

    pub fn mark_draft_sent(&self, id: i64) -> Result<()> {
        let now = Local::now().to_rfc3339();
        self.conn.execute(
            "UPDATE local_messages SET status=?2, sent_at=?3, error=NULL WHERE id=?1",
            params![id, STATUS_SENT, now],
        )?;
        Ok(())
    }

    /// Mark a local message as queued for upload to a remote Sent/Drafts
    /// folder. Called right before an `UploadRequest` is handed to the sync
    /// thread.
    pub fn set_upload_pending(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE local_messages SET upload_status=?2, upload_error=NULL WHERE id=?1",
            params![id, UPLOAD_PENDING],
        )?;
        Ok(())
    }

    pub fn set_upload_success(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE local_messages SET upload_status=?2, upload_error=NULL WHERE id=?1",
            params![id, UPLOAD_UPLOADED],
        )?;
        Ok(())
    }

    pub fn set_upload_error(&self, id: i64, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE local_messages SET upload_status=?2, upload_error=?3 WHERE id=?1",
            params![id, UPLOAD_ERROR, error],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn delete_draft(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM local_messages WHERE id=?1 AND status IN ('draft','sending','send_error')",
            params![id],
        )?;
        Ok(())
    }

    /// Drafts include in-flight ("sending") and failed ("send_error") rows
    /// so an in-progress or errored send never silently disappears from the
    /// Drafts list.
    pub fn get_drafts(&self, account: &str) -> Result<Vec<LocalMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, account, status, from_addr, to_addr, cc, bcc, subject, body,
                    in_reply_to, refs, created_at, sent_at, error, upload_status, upload_error
             FROM local_messages WHERE status IN ('draft','sending','send_error') AND account=?1
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![account], local_message_from_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn get_sent(&self, account: &str) -> Result<Vec<LocalMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, account, status, from_addr, to_addr, cc, bcc, subject, body,
                    in_reply_to, refs, created_at, sent_at, error, upload_status, upload_error
             FROM local_messages WHERE status='sent' AND account=?1
             ORDER BY sent_at DESC",
        )?;
        let rows = stmt.query_map(params![account], local_message_from_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    #[allow(dead_code)]
    pub fn get_draft(&self, id: i64) -> Result<Option<LocalMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, account, status, from_addr, to_addr, cc, bcc, subject, body,
                    in_reply_to, refs, created_at, sent_at, error, upload_status, upload_error
             FROM local_messages WHERE id=?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => Ok(Some(local_message_from_row(row)?)),
            None => Ok(None),
        }
    }
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct LocalMessage {
    pub id: i64,
    pub account: String,
    pub status: String,
    pub from_addr: String,
    pub to_addr: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub in_reply_to: String,
    pub refs: String,
    pub created_at: String,
    pub sent_at: Option<String>,
    pub error: Option<String>,
    pub upload_status: String,
    pub upload_error: Option<String>,
}

/// Human-readable in-flight/error state for a Drafts/Sent list row, or
/// `None` when there's nothing noteworthy to show beyond the plain
/// draft/sent marker already conveyed by the row's unread styling.
pub fn local_message_status_label(m: &LocalMessage) -> Option<String> {
    match m.status.as_str() {
        STATUS_SENDING => Some("Sending…".to_string()),
        STATUS_SEND_ERROR => Some(format!(
            "Send failed: {}",
            m.error.as_deref().unwrap_or("unknown error")
        )),
        STATUS_SENT => match m.upload_status.as_str() {
            UPLOAD_NONE => None,
            UPLOAD_PENDING => Some("Uploading to Sent…".to_string()),
            UPLOAD_UPLOADED => Some("Uploaded".to_string()),
            UPLOAD_ERROR => Some(format!(
                "Upload failed: {}",
                m.upload_error.as_deref().unwrap_or("unknown error")
            )),
            _ => None,
        },
        STATUS_DRAFT => match m.upload_status.as_str() {
            UPLOAD_NONE => None,
            UPLOAD_PENDING => Some("Uploading draft…".to_string()),
            UPLOAD_UPLOADED => Some("Draft uploaded".to_string()),
            UPLOAD_ERROR => Some(format!(
                "Draft upload failed: {}",
                m.upload_error.as_deref().unwrap_or("unknown error")
            )),
            _ => None,
        },
        _ => None,
    }
}

fn local_message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LocalMessage> {
    Ok(LocalMessage {
        id: row.get(0)?,
        account: row.get(1)?,
        status: row.get(2)?,
        from_addr: row.get(3)?,
        to_addr: row.get(4)?,
        cc: row.get(5)?,
        bcc: row.get(6)?,
        subject: row.get(7)?,
        body: row.get(8)?,
        in_reply_to: row.get(9)?,
        refs: row.get(10)?,
        created_at: row.get(11)?,
        sent_at: row.get(12)?,
        error: row.get(13)?,
        upload_status: row.get(14)?,
        upload_error: row.get(15)?,
    })
}

#[cfg(test)]
mod local_message_tests {
    use super::*;

    fn msg(status: &str, upload_status: &str) -> LocalMessage {
        LocalMessage {
            id: 1,
            account: "personal".to_string(),
            status: status.to_string(),
            from_addr: "alice@example.com".to_string(),
            to_addr: "bob@example.com".to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Hi".to_string(),
            body: "Hello".to_string(),
            in_reply_to: String::new(),
            refs: String::new(),
            created_at: "2026-01-01T00:00:00+00:00".to_string(),
            sent_at: None,
            error: None,
            upload_status: upload_status.to_string(),
            upload_error: None,
        }
    }

    #[test]
    fn plain_draft_has_no_status_label() {
        assert_eq!(
            local_message_status_label(&msg(STATUS_DRAFT, UPLOAD_NONE)),
            None
        );
    }

    #[test]
    fn sending_draft_shows_in_flight_label() {
        assert_eq!(
            local_message_status_label(&msg(STATUS_SENDING, UPLOAD_NONE)),
            Some("Sending…".to_string())
        );
    }

    #[test]
    fn send_error_label_includes_error_text() {
        let mut m = msg(STATUS_SEND_ERROR, UPLOAD_NONE);
        m.error = Some("connection refused".to_string());
        assert_eq!(
            local_message_status_label(&m),
            Some("Send failed: connection refused".to_string())
        );
    }

    #[test]
    fn sent_with_no_upload_configured_has_no_label() {
        assert_eq!(
            local_message_status_label(&msg(STATUS_SENT, UPLOAD_NONE)),
            None
        );
    }

    #[test]
    fn sent_upload_pending_shows_uploading_label() {
        assert_eq!(
            local_message_status_label(&msg(STATUS_SENT, UPLOAD_PENDING)),
            Some("Uploading to Sent…".to_string())
        );
    }

    #[test]
    fn sent_upload_error_includes_error_text() {
        let mut m = msg(STATUS_SENT, UPLOAD_ERROR);
        m.upload_error = Some("folder does not exist".to_string());
        assert_eq!(
            local_message_status_label(&m),
            Some("Upload failed: folder does not exist".to_string())
        );
    }

    #[test]
    fn draft_upload_states_produce_distinct_labels() {
        assert_eq!(
            local_message_status_label(&msg(STATUS_DRAFT, UPLOAD_PENDING)),
            Some("Uploading draft…".to_string())
        );
        assert_eq!(
            local_message_status_label(&msg(STATUS_DRAFT, UPLOAD_UPLOADED)),
            Some("Draft uploaded".to_string())
        );
    }

    #[test]
    fn drafts_query_includes_in_flight_and_errored_rows() {
        let db = MailDb::open_in_memory().unwrap();
        let id1 = db
            .save_draft("personal", "a@b.com", "c@d.com", "", "", "s1", "b1", "", "")
            .unwrap();
        let id2 = db
            .save_draft("personal", "a@b.com", "c@d.com", "", "", "s2", "b2", "", "")
            .unwrap();
        let id3 = db
            .save_draft("personal", "a@b.com", "c@d.com", "", "", "s3", "b3", "", "")
            .unwrap();
        db.mark_draft_sending(id2).unwrap();
        db.mark_draft_send_error(id3, "timeout").unwrap();

        let drafts = db.get_drafts("personal").unwrap();
        assert_eq!(drafts.len(), 3);

        db.mark_draft_sent(id1).unwrap();
        let drafts = db.get_drafts("personal").unwrap();
        assert_eq!(drafts.len(), 2);
        let sent = db.get_sent("personal").unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].id, id1);
    }

    #[test]
    fn upload_status_round_trips_through_db() {
        let db = MailDb::open_in_memory().unwrap();
        let id = db
            .save_draft("personal", "a@b.com", "c@d.com", "", "", "s", "b", "", "")
            .unwrap();
        db.mark_draft_sent(id).unwrap();
        db.set_upload_pending(id).unwrap();
        let sent = db.get_sent("personal").unwrap();
        assert_eq!(sent[0].upload_status, UPLOAD_PENDING);

        db.set_upload_error(id, "no such mailbox").unwrap();
        let sent = db.get_sent("personal").unwrap();
        assert_eq!(sent[0].upload_status, UPLOAD_ERROR);
        assert_eq!(sent[0].upload_error.as_deref(), Some("no such mailbox"));

        db.set_upload_success(id).unwrap();
        let sent = db.get_sent("personal").unwrap();
        assert_eq!(sent[0].upload_status, UPLOAD_UPLOADED);
        assert_eq!(sent[0].upload_error, None);
    }
}
