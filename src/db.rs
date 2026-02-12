use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::io::Read;
use std::path::PathBuf;

use crate::mail::{Email, EmailContent};
use chrono::{DateTime, Local, TimeZone};

pub struct MailDb {
    conn: Connection,
}

fn db_path() -> Result<PathBuf> {
    let data_dir = dirs::data_dir().context("Could not determine data directory")?;
    let dir = data_dir.join("jamail");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create {}", dir.display()))?;
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

impl MailDb {
    pub fn open() -> Result<Self> {
        let path = db_path()?;
        let conn = Connection::open(&path)
            .with_context(|| format!("Failed to open database at {}", path.display()))?;

        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;",
        )?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sync_state (
                id          INTEGER PRIMARY KEY CHECK (id = 1),
                account     TEXT NOT NULL,
                uidvalidity INTEGER NOT NULL DEFAULT 0,
                last_uid    INTEGER NOT NULL DEFAULT 0,
                last_sync   TEXT
            );

            CREATE TABLE IF NOT EXISTS emails (
                uid             INTEGER PRIMARY KEY,
                from_addr       TEXT NOT NULL,
                to_addr         TEXT NOT NULL DEFAULT '',
                subject         TEXT NOT NULL,
                date_ts         INTEGER NOT NULL,
                date_rfc3339    TEXT NOT NULL,
                is_unread       INTEGER NOT NULL DEFAULT 1,
                preview         TEXT NOT NULL DEFAULT '',
                text_body_zstd  BLOB,
                html_body_zstd  BLOB
            );

            CREATE INDEX IF NOT EXISTS idx_emails_date ON emails(date_ts DESC);

            CREATE VIRTUAL TABLE IF NOT EXISTS emails_fts USING fts5(
                from_addr, subject, body_text,
                tokenize='unicode61 remove_diacritics 2'
            );",
        )?;

        Ok(Self { conn })
    }

    pub fn store_email(
        &self,
        uid: u32,
        from: &str,
        to: &str,
        subject: &str,
        date: &DateTime<Local>,
        is_unread: bool,
        preview: &str,
        text_body: &str,
        html_body: Option<&str>,
    ) -> Result<()> {
        let date_ts = date.timestamp();
        let date_rfc3339 = date.to_rfc3339();
        let text_compressed = if text_body.is_empty() {
            None
        } else {
            Some(compress(text_body.as_bytes()))
        };
        let html_compressed = html_body.map(|h| compress(h.as_bytes()));

        self.conn.execute(
            "INSERT OR REPLACE INTO emails
                (uid, from_addr, to_addr, subject, date_ts, date_rfc3339,
                 is_unread, preview, text_body_zstd, html_body_zstd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
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
            ],
        )?;

        // Update FTS: delete old entry if exists, then insert
        self.conn
            .execute("DELETE FROM emails_fts WHERE rowid = ?1", params![uid])?;
        self.conn.execute(
            "INSERT INTO emails_fts (rowid, from_addr, subject, body_text) VALUES (?1, ?2, ?3, ?4)",
            params![uid, from, subject, text_body],
        )?;

        Ok(())
    }

    pub fn get_email_list(&self) -> Result<Vec<Email>> {
        let mut stmt = self.conn.prepare(
            "SELECT uid, from_addr, subject, date_ts, date_rfc3339, is_unread, preview
             FROM emails ORDER BY date_ts DESC",
        )?;

        let rows = stmt.query_map([], |row| {
            let uid: u32 = row.get(0)?;
            let from: String = row.get(1)?;
            let subject: String = row.get(2)?;
            let date_ts: i64 = row.get(3)?;
            let _date_rfc3339: String = row.get(4)?;
            let is_unread: bool = row.get::<_, i32>(5)? != 0;
            let preview: String = row.get(6)?;

            let date = Local
                .timestamp_opt(date_ts, 0)
                .single()
                .unwrap_or_else(Local::now);

            Ok(Email {
                uid,
                from,
                subject,
                date,
                is_unread,
                preview,
            })
        })?;

        let mut emails = Vec::new();
        for row in rows {
            emails.push(row?);
        }
        Ok(emails)
    }

    pub fn get_email_content(&self, uid: u32) -> Result<Option<EmailContent>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_addr, to_addr, subject, date_ts, text_body_zstd, html_body_zstd
             FROM emails WHERE uid = ?1",
        )?;

        let mut rows = stmt.query(params![uid])?;
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

        let date = Local
            .timestamp_opt(date_ts, 0)
            .single()
            .unwrap_or_else(Local::now);

        let text_body = text_zstd
            .map(|z| String::from_utf8_lossy(&decompress(&z)).to_string())
            .unwrap_or_default();

        let html_body = html_zstd.map(|z| String::from_utf8_lossy(&decompress(&z)).to_string());

        Ok(Some(EmailContent {
            from,
            to,
            subject,
            date,
            text_body,
            html_body,
        }))
    }

    pub fn mark_read(&self, uid: u32) -> Result<()> {
        self.conn
            .execute("UPDATE emails SET is_unread = 0 WHERE uid = ?1", params![uid])?;
        Ok(())
    }

    pub fn search_emails(&self, query: &str) -> Result<Vec<Email>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.uid, e.from_addr, e.subject, e.date_ts, e.is_unread, e.preview
             FROM emails_fts f
             JOIN emails e ON e.uid = f.rowid
             WHERE emails_fts MATCH ?1
             ORDER BY e.date_ts DESC",
        )?;

        let rows = stmt.query_map(params![query], |row| {
            let uid: u32 = row.get(0)?;
            let from: String = row.get(1)?;
            let subject: String = row.get(2)?;
            let date_ts: i64 = row.get(3)?;
            let is_unread: bool = row.get::<_, i32>(4)? != 0;
            let preview: String = row.get(5)?;

            let date = Local
                .timestamp_opt(date_ts, 0)
                .single()
                .unwrap_or_else(Local::now);

            Ok(Email {
                uid,
                from,
                subject,
                date,
                is_unread,
                preview,
            })
        })?;

        let mut emails = Vec::new();
        for row in rows {
            emails.push(row?);
        }
        Ok(emails)
    }

    #[allow(dead_code)]
    pub fn has_email(&self, uid: u32) -> Result<bool> {
        let count: i32 = self.conn.query_row(
            "SELECT COUNT(*) FROM emails WHERE uid = ?1",
            params![uid],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    #[allow(dead_code)]
    pub fn email_count(&self) -> Result<u32> {
        let count: u32 = self
            .conn
            .query_row("SELECT COUNT(*) FROM emails", [], |row| row.get(0))?;
        Ok(count)
    }

    pub fn get_sync_state(&self, account: &str) -> Result<Option<(u32, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT uidvalidity, last_uid FROM sync_state WHERE id = 1 AND account = ?1",
        )?;
        let mut rows = stmt.query(params![account])?;
        match rows.next()? {
            Some(row) => {
                let uidvalidity: u32 = row.get(0)?;
                let last_uid: u32 = row.get(1)?;
                Ok(Some((uidvalidity, last_uid)))
            }
            None => Ok(None),
        }
    }

    pub fn set_sync_state(&self, account: &str, uidvalidity: u32, last_uid: u32) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sync_state (id, account, uidvalidity, last_uid, last_sync)
             VALUES (1, ?1, ?2, ?3, datetime('now'))",
            params![account, uidvalidity, last_uid],
        )?;
        Ok(())
    }

    pub fn clear_all_emails(&self) -> Result<()> {
        self.conn.execute("DELETE FROM emails", [])?;
        self.conn.execute("DELETE FROM emails_fts", [])?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn begin_transaction(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn commit_transaction(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }
}
