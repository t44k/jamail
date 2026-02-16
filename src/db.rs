use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::io::Read;
use std::path::PathBuf;

use crate::mail::{AttachmentData, Email, EmailContent, FolderInfo};
use chrono::{DateTime, Local, TimeZone};

const CURRENT_SCHEMA_VERSION: i32 = 2;

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
         DROP TABLE IF EXISTS schema_version;

         CREATE TABLE schema_version (version INTEGER NOT NULL);
         INSERT INTO schema_version VALUES (2);

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
         CREATE INDEX idx_attachments_email_id ON attachments(email_id);",
    )?;
    Ok(())
}

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
                    message_id, in_reply_to, refs, has_attachments
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

            let date = Local
                .timestamp_opt(date_ts, 0)
                .single()
                .unwrap_or_else(Local::now);

            Ok(Email {
                id,
                uid,
                from,
                subject,
                date,
                is_unread,
                preview,
                message_id,
                in_reply_to,
                references,
                has_attachments,
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

    pub fn mark_read(&self, id: i64) -> Result<()> {
        self.conn
            .execute("UPDATE emails SET is_unread = 0 WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn search_emails(&self, account: &str, folder: &str, query: &str) -> Result<Vec<Email>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.id, e.uid, e.from_addr, e.subject, e.date_ts, e.is_unread, e.preview,
                    e.message_id, e.in_reply_to, e.refs, e.has_attachments
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

            let date = Local
                .timestamp_opt(date_ts, 0)
                .single()
                .unwrap_or_else(Local::now);

            Ok(Email {
                id,
                uid,
                from,
                subject,
                date,
                is_unread,
                preview,
                message_id,
                in_reply_to,
                references,
                has_attachments,
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
}
