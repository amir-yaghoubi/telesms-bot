use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;

const PENDING_FORWARD_TTL_MINUTES: i64 = 10;

/// Inbound and outbound rows as one message stream; outbound rows carry no
/// classification.
const MESSAGE_UNION: &str = "SELECT id, direction, e164, body, ts, sms_ts, status, tid,
            category, confidence, tags, otp_code
     FROM (
       SELECT id, 'in' AS direction, e164, body, created_at AS ts, sms_ts,
              'ok' AS status, COALESCE(thread_id, 1) AS tid,
              category, confidence, tags, otp_code
       FROM inbound_log
       UNION ALL
       SELECT id, 'out', e164, body, created_at, NULL,
              CASE WHEN result = 'ok' THEN 'ok' ELSE 'failed' END,
              COALESCE(thread_id, 1), NULL, NULL, NULL, NULL
       FROM outbound_log
     )";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS contacts (
  id INTEGER PRIMARY KEY,
  google_resource TEXT UNIQUE NOT NULL,
  display_name TEXT NOT NULL,
  last_synced TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS contact_numbers (
  contact_id INTEGER NOT NULL REFERENCES contacts(id),
  e164 TEXT NOT NULL,
  last_seen_incoming TEXT,
  UNIQUE(contact_id, e164)
);
CREATE INDEX IF NOT EXISTS idx_numbers_e164 ON contact_numbers(e164);
CREATE TABLE IF NOT EXISTS topics (
  thread_id INTEGER PRIMARY KEY,
  contact_id INTEGER REFERENCES contacts(id),
  default_e164 TEXT,
  title TEXT NOT NULL,
  ignored INTEGER NOT NULL DEFAULT 0,
  pending_outbound TEXT,
  pending_reply_to INTEGER
);
CREATE TABLE IF NOT EXISTS number_ignore (
  e164 TEXT PRIMARY KEY
);
CREATE TABLE IF NOT EXISTS inbound_log (
  id INTEGER PRIMARY KEY,
  mm_path TEXT UNIQUE NOT NULL,
  e164 TEXT NOT NULL,
  body TEXT NOT NULL,
  tg_msg INTEGER,
  created_at TEXT NOT NULL,
  sms_ts TEXT
);
CREATE TABLE IF NOT EXISTS outbound_log (
  id INTEGER PRIMARY KEY,
  e164 TEXT NOT NULL,
  body TEXT NOT NULL,
  result TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_inbound_tg_msg ON inbound_log(tg_msg);
CREATE TABLE IF NOT EXISTS otp_patterns (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  pattern TEXT NOT NULL UNIQUE,
  created_at TEXT NOT NULL
);
";

#[derive(Debug, Error)]
pub enum DbError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database lock poisoned")]
    Poisoned,
    #[error("contacts unavailable")]
    ContactsUnavailable,
    #[error("inbound row missing: {0}")]
    MissingInboundRow(String),
}

pub struct Db {
    conn: Mutex<Connection>,
    contacts_available: AtomicBool,
}

#[derive(Clone, Debug)]
pub struct Contact {
    pub id: i64,
    pub google_resource: String,
    pub display_name: String,
    pub numbers: Vec<String>,
    pub ambiguous: bool,
}

#[derive(Clone, Debug)]
pub struct Topic {
    pub thread_id: i32,
    pub contact_id: Option<i64>,
    pub default_e164: Option<String>,
    pub title: String,
    pub ignored: bool,
    pub sort: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ChatSummary {
    pub thread_id: i32,
    pub title: String,
    pub contact_id: Option<i64>,
    pub display_name: Option<String>,
    pub default_e164: Option<String>,
    pub last_message_at: String,
    pub last_message_preview: String,
    pub last_message_direction: String,
    pub sort: Option<String>,
}

#[derive(Clone, Debug)]
pub struct MessageRow {
    pub id: String,
    pub direction: String,
    pub e164: String,
    pub body: String,
    pub timestamp: String,
    pub sms_ts: Option<String>,
    pub status: String,
    pub thread_id: i32,
    pub classification: Option<Classification>,
    pub otp_code: Option<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Classification {
    pub category: String,
    pub confidence: f64,
    pub tags: Vec<String>,
}

/// A classification as persisted: the public part plus the raw model scores.
#[derive(Clone, Debug)]
pub struct StoredClassification {
    pub public: Classification,
    pub scores: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct OtpPatternRow {
    pub id: i64,
    pub name: String,
    pub pattern: String,
    pub created_at: String,
}

#[derive(Debug, Default)]
pub struct MessageFilter {
    pub category: Option<String>,
    pub tag: Option<String>,
}

pub struct TodaySmsCounts {
    pub inbound: u32,
    pub sent_ok: u32,
    pub sent_fail: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingForwardMode {
    Search,
    Number,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingForward {
    pub mode: PendingForwardMode,
    pub edit_chat_id: i64,
    pub edit_message_id: i32,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, DbError> {
        let conn = Connection::open(path)?;
        Self::from_conn(conn)
    }

    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        Self::from_conn(conn)
    }

    fn from_conn(conn: Connection) -> Result<Self, DbError> {
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        let _ = conn.execute("ALTER TABLE topics ADD COLUMN pending_outbound TEXT", []);
        let _ = conn.execute("ALTER TABLE topics ADD COLUMN pending_reply_to INTEGER", []);
        let _ = conn.execute("ALTER TABLE inbound_log ADD COLUMN sms_ts TEXT", []);
        let _ = conn.execute("ALTER TABLE inbound_log ADD COLUMN thread_id INTEGER", []);
        let _ = conn.execute("ALTER TABLE outbound_log ADD COLUMN thread_id INTEGER", []);
        for column in [
            "category TEXT",
            "confidence REAL",
            "tags TEXT",
            "scores TEXT",
            "classify_status TEXT",
            "classify_attempts INTEGER NOT NULL DEFAULT 0",
            "otp_code TEXT",
        ] {
            let _ = conn.execute(&format!("ALTER TABLE inbound_log ADD COLUMN {column}"), []);
        }
        let _ = conn.execute("ALTER TABLE topics ADD COLUMN sort TEXT", []);
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pending_forward (
                thread_id INTEGER PRIMARY KEY,
                mode TEXT NOT NULL,
                edit_chat_id INTEGER NOT NULL,
                edit_message_id INTEGER NOT NULL,
                created_at TEXT NOT NULL
             );",
        )?;
        let _ = conn.execute("ALTER TABLE pending_forward ADD COLUMN created_at TEXT", []);
        conn.execute(
            "UPDATE pending_forward SET created_at = ?1 WHERE created_at IS NULL",
            [Self::now()],
        )?;
        let db = Db {
            conn: Mutex::new(conn),
            contacts_available: AtomicBool::new(true),
        };
        db.backfill_thread_ids()?;
        Ok(db)
    }

    pub fn set_contacts_available(&self, available: bool) {
        self.contacts_available.store(available, Ordering::SeqCst);
    }

    pub fn contacts_available(&self) -> bool {
        self.contacts_available.load(Ordering::SeqCst)
    }

    fn conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, DbError> {
        self.conn.lock().map_err(|_| DbError::Poisoned)
    }

    fn now() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    pub fn upsert_contact(
        &self,
        google_resource: &str,
        display_name: &str,
    ) -> Result<i64, DbError> {
        let conn = self.conn()?;
        let id = conn.query_row(
            "INSERT INTO contacts (google_resource, display_name, last_synced)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(google_resource) DO UPDATE SET
               display_name = excluded.display_name,
               last_synced = excluded.last_synced
             RETURNING id",
            rusqlite::params![google_resource, display_name, Self::now()],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn replace_contact_numbers(
        &self,
        contact_id: i64,
        e164s: &[String],
    ) -> Result<(), DbError> {
        let conn = self.conn()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM contact_numbers WHERE contact_id = ?1",
            rusqlite::params![contact_id],
        )?;
        {
            let mut stmt =
                tx.prepare("INSERT INTO contact_numbers (contact_id, e164) VALUES (?1, ?2)")?;
            for e164 in e164s {
                stmt.execute(rusqlite::params![contact_id, e164])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn find_contact_by_e164(&self, e164: &str) -> Result<Option<Contact>, DbError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT c.id, c.google_resource, c.display_name
             FROM contacts c
             JOIN contact_numbers n ON n.contact_id = c.id
             WHERE n.e164 = ?1
             ORDER BY c.id ASC",
        )?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map(rusqlite::params![e164], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let Some((id, google_resource, display_name)) = rows.first().cloned() else {
            return Ok(None);
        };
        let numbers = Self::load_numbers(&conn, id)?;
        Ok(Some(Contact {
            id,
            google_resource,
            display_name,
            numbers,
            ambiguous: rows.len() > 1,
        }))
    }

    pub fn search_contacts(&self, query: &str) -> Result<Vec<Contact>, DbError> {
        if !self.contacts_available.load(Ordering::SeqCst) {
            return Err(DbError::ContactsUnavailable);
        }
        let conn = self.conn()?;
        let pattern = format!("%{query}%");
        let mut stmt = conn.prepare(
            "SELECT id, google_resource, display_name
             FROM contacts
             WHERE display_name LIKE ?1 COLLATE NOCASE
             ORDER BY id ASC",
        )?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map(rusqlite::params![pattern], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut contacts = Vec::with_capacity(rows.len());
        for (id, google_resource, display_name) in rows {
            let numbers = Self::load_numbers(&conn, id)?;
            contacts.push(Contact {
                id,
                google_resource,
                display_name,
                numbers,
                ambiguous: false,
            });
        }
        Ok(contacts)
    }

    pub fn get_contact(&self, id: i64) -> Result<Option<Contact>, DbError> {
        let conn = self.conn()?;
        let row: Option<(i64, String, String)> = conn
            .query_row(
                "SELECT id, google_resource, display_name FROM contacts WHERE id = ?1",
                rusqlite::params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((id, google_resource, display_name)) = row else {
            return Ok(None);
        };
        let numbers = Self::load_numbers(&conn, id)?;
        Ok(Some(Contact {
            id,
            google_resource,
            display_name,
            numbers,
            ambiguous: false,
        }))
    }

    pub fn contact_numbers(&self, contact_id: i64) -> Result<Vec<String>, DbError> {
        let conn = self.conn()?;
        Self::load_numbers(&conn, contact_id)
    }

    fn load_numbers(conn: &Connection, contact_id: i64) -> Result<Vec<String>, DbError> {
        let mut stmt =
            conn.prepare("SELECT e164 FROM contact_numbers WHERE contact_id = ?1 ORDER BY rowid")?;
        let numbers = stmt
            .query_map(rusqlite::params![contact_id], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(numbers)
    }

    pub fn upsert_topic(&self, topic: &Topic) -> Result<(), DbError> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO topics (thread_id, contact_id, default_e164, title, ignored)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(thread_id) DO UPDATE SET
               contact_id = excluded.contact_id,
               default_e164 = excluded.default_e164,
               title = excluded.title,
               ignored = excluded.ignored",
            rusqlite::params![
                topic.thread_id,
                topic.contact_id,
                topic.default_e164,
                topic.title,
                topic.ignored as i64,
            ],
        )?;
        Ok(())
    }

    pub fn set_pending_outbound(
        &self,
        thread_id: i32,
        text: &str,
        reply_to: Option<i32>,
    ) -> Result<(), DbError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE topics SET pending_outbound = ?1, pending_reply_to = ?2 WHERE thread_id = ?3",
            rusqlite::params![text, reply_to, thread_id],
        )?;
        Ok(())
    }

    pub fn take_pending_outbound(
        &self,
        thread_id: i32,
    ) -> Result<Option<(String, Option<i32>)>, DbError> {
        let conn = self.conn()?;
        let row: Option<(Option<String>, Option<i32>)> = conn
            .query_row(
                "SELECT pending_outbound, pending_reply_to FROM topics WHERE thread_id = ?1",
                rusqlite::params![thread_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        conn.execute(
            "UPDATE topics SET pending_outbound = NULL, pending_reply_to = NULL WHERE thread_id = ?1",
            rusqlite::params![thread_id],
        )?;
        Ok(row.and_then(|(text, reply_to)| text.map(|t| (t, reply_to))))
    }

    fn pending_forward_mode_str(mode: PendingForwardMode) -> &'static str {
        match mode {
            PendingForwardMode::Search => "search",
            PendingForwardMode::Number => "number",
        }
    }

    fn pending_forward_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PendingForward> {
        let mode: String = row.get(0)?;
        let mode = match mode.as_str() {
            "search" => PendingForwardMode::Search,
            "number" => PendingForwardMode::Number,
            other => {
                return Err(rusqlite::Error::InvalidColumnType(
                    0,
                    other.to_string(),
                    rusqlite::types::Type::Text,
                ));
            }
        };
        Ok(PendingForward {
            mode,
            edit_chat_id: row.get(1)?,
            edit_message_id: row.get(2)?,
        })
    }

    pub fn set_pending_forward(
        &self,
        thread_id: i32,
        mode: PendingForwardMode,
        edit_chat_id: i64,
        edit_message_id: i32,
    ) -> Result<(), DbError> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO pending_forward
               (thread_id, mode, edit_chat_id, edit_message_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(thread_id) DO UPDATE SET
               mode = excluded.mode,
               edit_chat_id = excluded.edit_chat_id,
               edit_message_id = excluded.edit_message_id,
               created_at = excluded.created_at",
            rusqlite::params![
                thread_id,
                Self::pending_forward_mode_str(mode),
                edit_chat_id,
                edit_message_id,
                Self::now(),
            ],
        )?;
        Ok(())
    }

    pub fn get_pending_forward(&self, thread_id: i32) -> Result<Option<PendingForward>, DbError> {
        let conn = self.conn()?;
        let cutoff = (chrono::Utc::now() - chrono::Duration::minutes(PENDING_FORWARD_TTL_MINUTES))
            .to_rfc3339();
        conn.query_row(
            "SELECT mode, edit_chat_id, edit_message_id
             FROM pending_forward
             WHERE thread_id = ?1 AND unixepoch(created_at) >= unixepoch(?2)",
            rusqlite::params![thread_id, cutoff],
            Self::pending_forward_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn take_pending_forward(&self, thread_id: i32) -> Result<Option<PendingForward>, DbError> {
        let conn = self.conn()?;
        let cutoff = (chrono::Utc::now() - chrono::Duration::minutes(PENDING_FORWARD_TTL_MINUTES))
            .to_rfc3339();
        let pending = conn
            .query_row(
                "SELECT mode, edit_chat_id, edit_message_id
                 FROM pending_forward
                 WHERE thread_id = ?1 AND unixepoch(created_at) >= unixepoch(?2)",
                rusqlite::params![thread_id, cutoff],
                Self::pending_forward_from_row,
            )
            .optional()?;
        conn.execute(
            "DELETE FROM pending_forward WHERE thread_id = ?1",
            rusqlite::params![thread_id],
        )?;
        Ok(pending)
    }

    pub fn clear_pending_forward(&self, thread_id: i32) -> Result<(), DbError> {
        let conn = self.conn()?;
        conn.execute(
            "DELETE FROM pending_forward WHERE thread_id = ?1",
            rusqlite::params![thread_id],
        )?;
        Ok(())
    }

    pub fn set_default_number(&self, thread_id: i32, e164: &str) -> Result<(), DbError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE topics SET default_e164 = ?1 WHERE thread_id = ?2",
            rusqlite::params![e164, thread_id],
        )?;
        Ok(())
    }

    pub fn get_topic_by_thread(&self, thread_id: i32) -> Result<Option<Topic>, DbError> {
        self.get_topic(
            "SELECT thread_id, contact_id, default_e164, title, ignored, sort
             FROM topics WHERE thread_id = ?1",
            rusqlite::params![thread_id],
        )
    }

    pub fn get_topic_by_contact(&self, contact_id: i64) -> Result<Option<Topic>, DbError> {
        self.get_topic(
            "SELECT thread_id, contact_id, default_e164, title, ignored, sort
             FROM topics WHERE contact_id = ?1
             ORDER BY thread_id ASC
             LIMIT 1",
            rusqlite::params![contact_id],
        )
    }

    pub fn get_topic_by_e164(&self, e164: &str) -> Result<Option<Topic>, DbError> {
        self.get_topic(
            "SELECT thread_id, contact_id, default_e164, title, ignored, sort
             FROM topics WHERE default_e164 = ?1
             ORDER BY thread_id ASC
             LIMIT 1",
            rusqlite::params![e164],
        )
    }

    fn get_topic(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Option<Topic>, DbError> {
        let conn = self.conn()?;
        let topic = conn
            .query_row(sql, params, Self::topic_from_row)
            .optional()?;
        Ok(topic)
    }

    fn topic_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Topic> {
        Ok(Topic {
            thread_id: row.get(0)?,
            contact_id: row.get(1)?,
            default_e164: row.get(2)?,
            title: row.get(3)?,
            ignored: row.get::<_, i64>(4)? != 0,
            sort: row.get(5)?,
        })
    }

    pub fn ignore_number(&self, e164: &str) -> Result<(), DbError> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT OR IGNORE INTO number_ignore (e164) VALUES (?1)",
            rusqlite::params![e164],
        )?;
        Ok(())
    }

    pub fn is_ignored(&self, e164: &str) -> Result<bool, DbError> {
        let conn = self.conn()?;
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM number_ignore WHERE e164 = ?1",
                rusqlite::params![e164],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    pub fn seen_sms_path(&self, path: &str) -> Result<bool, DbError> {
        let conn = self.conn()?;
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM inbound_log WHERE mm_path = ?1",
                rusqlite::params![path],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Dedup key is the message content: number + body + modem timestamp.
    /// D-Bus SMS paths change when ModemManager reloads the inbox and are
    /// REUSED from low numbers after every ModemManager restart, so a path
    /// match may refer to an unrelated message seen weeks earlier; matching
    /// by path silently drops every new SMS after a reboot.
    pub fn seen_sms(
        &self,
        _path: &str,
        e164: &str,
        text: &str,
        sms_ts: &str,
    ) -> Result<bool, DbError> {
        if sms_ts.is_empty() {
            return Ok(false);
        }
        let conn = self.conn()?;
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM inbound_log
                 WHERE e164 = ?1 AND body = ?2 AND sms_ts = ?3",
                rusqlite::params![e164, text, sms_ts],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    pub fn mark_incoming(&self, e164: &str) -> Result<(), DbError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE contact_numbers SET last_seen_incoming = ?1 WHERE e164 = ?2",
            rusqlite::params![Self::now(), e164],
        )?;
        Ok(())
    }

    /// Record an inbound message and return the row it wrote.
    ///
    /// mm_path is a recycled ModemManager slot id (renumbered from 0 at every
    /// restart), so a stale row may already hold the path for an unrelated
    /// older message; on conflict the row is refreshed to the new message.
    /// Dedup itself is content-based via `seen_sms`.
    #[allow(clippy::too_many_arguments)]
    pub fn record_inbound(
        &self,
        path: &str,
        e164: &str,
        text: &str,
        tg_msg: Option<i32>,
        sms_ts: &str,
        thread_id: i32,
        otp_code: Option<&str>,
        classified: Option<&StoredClassification>,
    ) -> Result<MessageRow, DbError> {
        let created_at = Self::now();
        let category = classified.map(|c| c.public.category.as_str());
        let confidence = classified.map(|c| c.public.confidence);
        let tags = classified.map(|c| serde_json::to_string(&c.public.tags).unwrap_or_default());
        let scores = classified.map(|c| c.scores.to_string());
        let status = if classified.is_some() {
            "done"
        } else {
            "pending"
        };
        {
            let conn = self.conn()?;
            conn.execute(
                "INSERT INTO inbound_log
                   (mm_path, e164, body, tg_msg, created_at, sms_ts, thread_id,
                    category, confidence, tags, scores, classify_status,
                    classify_attempts, otp_code)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0, ?13)
                 ON CONFLICT(mm_path) DO UPDATE SET
                   e164 = excluded.e164,
                   body = excluded.body,
                   tg_msg = excluded.tg_msg,
                   created_at = excluded.created_at,
                   sms_ts = excluded.sms_ts,
                   thread_id = excluded.thread_id,
                   category = excluded.category,
                   confidence = excluded.confidence,
                   tags = excluded.tags,
                   scores = excluded.scores,
                   classify_status = excluded.classify_status,
                   classify_attempts = 0,
                   otp_code = excluded.otp_code",
                rusqlite::params![
                    path, e164, text, tg_msg, created_at, sms_ts, thread_id, category, confidence,
                    tags, scores, status, otp_code,
                ],
            )?;
        }
        let row_id = self.inbound_row_id(path)?;
        Ok(MessageRow {
            id: format!("in:{row_id}"),
            direction: "in".into(),
            e164: e164.to_string(),
            body: text.to_string(),
            timestamp: created_at,
            sms_ts: Some(sms_ts.to_string()).filter(|value| !value.is_empty()),
            status: "ok".into(),
            thread_id,
            classification: classified.map(|c| c.public.clone()),
            otp_code: otp_code.map(str::to_string),
        })
    }

    /// Record an outbound message and return the row it wrote.
    pub fn record_outbound(
        &self,
        e164: &str,
        text: &str,
        result: &str,
        thread_id: i32,
    ) -> Result<MessageRow, DbError> {
        let created_at = Self::now();
        let row_id = {
            let conn = self.conn()?;
            conn.execute(
                "INSERT INTO outbound_log (e164, body, result, created_at, thread_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![e164, text, result, created_at, thread_id],
            )?;
            conn.last_insert_rowid()
        };
        Ok(MessageRow {
            id: format!("out:{row_id}"),
            direction: "out".into(),
            e164: e164.to_string(),
            body: text.to_string(),
            timestamp: created_at,
            sms_ts: None,
            status: if result == "ok" { "ok" } else { "failed" }.into(),
            thread_id,
            classification: None,
            otp_code: None,
        })
    }

    pub fn set_classification(
        &self,
        inbound_id: i64,
        c: &StoredClassification,
    ) -> Result<Option<MessageRow>, DbError> {
        let tags = serde_json::to_string(&c.public.tags).unwrap_or_default();
        {
            let conn = self.conn()?;
            let changed = conn.execute(
                "UPDATE inbound_log
                 SET category = ?2, confidence = ?3, tags = ?4, scores = ?5,
                     classify_status = 'done'
                 WHERE id = ?1",
                rusqlite::params![
                    inbound_id,
                    c.public.category,
                    c.public.confidence,
                    tags,
                    c.scores.to_string(),
                ],
            )?;
            if changed == 0 {
                return Ok(None);
            }
        }
        Ok(self.inbound_by_id(inbound_id)?.map(|(row, _)| row))
    }

    /// Count a failed classification attempt; at `max_attempts` the row is
    /// marked `failed` and stops being retried.
    pub fn record_classify_failure(
        &self,
        inbound_id: i64,
        max_attempts: u32,
    ) -> Result<(), DbError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE inbound_log
             SET classify_attempts = classify_attempts + 1,
                 classify_status = CASE
                   WHEN classify_attempts + 1 >= ?2 THEN 'failed'
                   ELSE COALESCE(classify_status, 'pending')
                 END
             WHERE id = ?1",
            rusqlite::params![inbound_id, max_attempts],
        )?;
        Ok(())
    }

    /// Inbound messages awaiting classification as (id, e164, body), oldest
    /// first. A NULL status (recorded before classification existed) counts
    /// as `pending`.
    pub fn pending_classification(
        &self,
        since: Option<&str>,
        statuses: &[&str],
        max_attempts: Option<u32>,
        limit: i64,
    ) -> Result<Vec<(i64, String, String)>, DbError> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = (0..statuses.len())
            .map(|i| format!("?{}", i + 4))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT id, e164, body FROM inbound_log
             WHERE (?1 IS NULL OR created_at >= ?1)
               AND (?2 IS NULL OR classify_attempts < ?2)
               AND COALESCE(classify_status, 'pending') IN ({placeholders})
             ORDER BY created_at ASC, id ASC
             LIMIT ?3"
        );
        let mut params: Vec<rusqlite::types::Value> = vec![
            since.map(str::to_string).into(),
            max_attempts.map(i64::from).into(),
            limit.into(),
        ];
        params.extend(
            statuses
                .iter()
                .map(|s| rusqlite::types::Value::from(s.to_string())),
        );
        let conn = self.conn()?;
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn set_otp_code(&self, inbound_id: i64, code: &str) -> Result<Option<MessageRow>, DbError> {
        {
            let conn = self.conn()?;
            let changed = conn.execute(
                "UPDATE inbound_log SET otp_code = ?2 WHERE id = ?1",
                rusqlite::params![inbound_id, code],
            )?;
            if changed == 0 {
                return Ok(None);
            }
        }
        Ok(self.inbound_by_id(inbound_id)?.map(|(row, _)| row))
    }

    fn inbound_where(
        &self,
        condition: &str,
        param: i64,
    ) -> Result<Option<(MessageRow, Option<i32>)>, DbError> {
        let conn = self.conn()?;
        let found = conn
            .query_row(
                &format!(
                    "SELECT id, e164, body, created_at, sms_ts, COALESCE(thread_id, 1),
                            category, confidence, tags, otp_code, tg_msg
                     FROM inbound_log WHERE {condition} ORDER BY id DESC LIMIT 1"
                ),
                rusqlite::params![param],
                |row| {
                    let id: i64 = row.get(0)?;
                    let sms_ts: Option<String> = row.get(4)?;
                    let message = MessageRow {
                        id: format!("in:{id}"),
                        direction: "in".into(),
                        e164: row.get(1)?,
                        body: row.get(2)?,
                        timestamp: row.get(3)?,
                        sms_ts: sms_ts.filter(|value| !value.is_empty()),
                        status: "ok".into(),
                        thread_id: row.get(5)?,
                        classification: Self::classification_from(
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                        ),
                        otp_code: row.get(9)?,
                    };
                    Ok((message, row.get(10)?))
                },
            )
            .optional()?;
        Ok(found)
    }

    /// The inbound row and the Telegram message id it was forwarded as.
    pub fn inbound_by_id(
        &self,
        inbound_id: i64,
    ) -> Result<Option<(MessageRow, Option<i32>)>, DbError> {
        self.inbound_where("id = ?1", inbound_id)
    }

    pub fn inbound_by_tg_msg(&self, tg_msg: i32) -> Result<Option<MessageRow>, DbError> {
        Ok(self
            .inbound_where("tg_msg = ?1", i64::from(tg_msg))?
            .map(|(row, _)| row))
    }

    fn classification_from(
        category: Option<String>,
        confidence: Option<f64>,
        tags: Option<String>,
    ) -> Option<Classification> {
        category.map(|category| Classification {
            category,
            confidence: confidence.unwrap_or_default(),
            tags: tags
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default(),
        })
    }

    pub fn otp_patterns(&self) -> Result<Vec<OtpPatternRow>, DbError> {
        let conn = self.conn()?;
        let mut stmt =
            conn.prepare("SELECT id, name, pattern, created_at FROM otp_patterns ORDER BY id")?;
        let rows = stmt
            .query_map([], Self::otp_pattern_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn otp_pattern_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OtpPatternRow> {
        Ok(OtpPatternRow {
            id: row.get(0)?,
            name: row.get(1)?,
            pattern: row.get(2)?,
            created_at: row.get(3)?,
        })
    }

    /// Store a pattern. The bool is true when it was newly created; a pattern
    /// that already exists is returned unchanged. The caller validates.
    pub fn add_otp_pattern(
        &self,
        name: Option<&str>,
        pattern: &str,
    ) -> Result<(OtpPatternRow, bool), DbError> {
        let conn = self.conn()?;
        let created = conn.execute(
            "INSERT OR IGNORE INTO otp_patterns (name, pattern, created_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![name.unwrap_or_default(), pattern, Self::now()],
        )? > 0;
        if created && name.is_none() {
            conn.execute(
                "UPDATE otp_patterns SET name = 'pattern-' || id WHERE id = ?1",
                rusqlite::params![conn.last_insert_rowid()],
            )?;
        }
        let row = conn.query_row(
            "SELECT id, name, pattern, created_at FROM otp_patterns WHERE pattern = ?1",
            rusqlite::params![pattern],
            Self::otp_pattern_from_row,
        )?;
        Ok((row, created))
    }

    pub fn delete_otp_pattern(&self, id: i64) -> Result<bool, DbError> {
        let conn = self.conn()?;
        let changed = conn.execute(
            "DELETE FROM otp_patterns WHERE id = ?1",
            rusqlite::params![id],
        )?;
        Ok(changed > 0)
    }

    pub fn sorted_topic(&self, sort: &str) -> Result<Option<i32>, DbError> {
        let conn = self.conn()?;
        let thread_id = conn
            .query_row(
                "SELECT thread_id FROM topics WHERE sort = ?1 ORDER BY thread_id ASC LIMIT 1",
                rusqlite::params![sort],
                |row| row.get(0),
            )
            .optional()?;
        Ok(thread_id)
    }

    /// Make `thread_id` the topic for `sort`, releasing any earlier one.
    pub fn upsert_sorted_topic(
        &self,
        sort: &str,
        thread_id: i32,
        title: &str,
    ) -> Result<(), DbError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE topics SET sort = NULL WHERE sort = ?1 AND thread_id <> ?2",
            rusqlite::params![sort, thread_id],
        )?;
        conn.execute(
            "INSERT INTO topics (thread_id, title, sort) VALUES (?1, ?2, ?3)
             ON CONFLICT(thread_id) DO UPDATE SET
               title = excluded.title,
               sort = excluded.sort",
            rusqlite::params![thread_id, title, sort],
        )?;
        Ok(())
    }

    fn inbound_row_id(&self, path: &str) -> Result<i64, DbError> {
        let conn = self.conn()?;
        conn.query_row(
            "SELECT id FROM inbound_log WHERE mm_path = ?1",
            [path],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(DbError::MissingInboundRow(path.to_string()))
    }

    pub fn backfill_thread_ids(&self) -> Result<u64, DbError> {
        let conn = self.conn()?;
        let mut updated = 0u64;
        updated += conn.execute(
            "UPDATE inbound_log
             SET thread_id = (
               SELECT t.thread_id FROM topics t
               WHERE t.default_e164 = inbound_log.e164
               ORDER BY t.thread_id ASC LIMIT 1
             )
             WHERE thread_id IS NULL",
            [],
        )? as u64;
        updated += conn.execute(
            "UPDATE inbound_log
             SET thread_id = (
               SELECT t.thread_id
               FROM contact_numbers n
               JOIN topics t ON t.contact_id = n.contact_id
               WHERE n.e164 = inbound_log.e164
               ORDER BY t.thread_id ASC LIMIT 1
             )
             WHERE thread_id IS NULL",
            [],
        )? as u64;
        updated += conn.execute(
            "UPDATE outbound_log
             SET thread_id = (
               SELECT t.thread_id FROM topics t
               WHERE t.default_e164 = outbound_log.e164
               ORDER BY t.thread_id ASC LIMIT 1
             )
             WHERE thread_id IS NULL",
            [],
        )? as u64;
        updated += conn.execute(
            "UPDATE outbound_log
             SET thread_id = (
               SELECT t.thread_id
               FROM contact_numbers n
               JOIN topics t ON t.contact_id = n.contact_id
               WHERE n.e164 = outbound_log.e164
               ORDER BY t.thread_id ASC LIMIT 1
             )
             WHERE thread_id IS NULL",
            [],
        )? as u64;
        Ok(updated)
    }

    pub fn chats_with_activity(
        &self,
        limit: i64,
        before: Option<&str>,
        after: Option<&str>,
    ) -> Result<Vec<ChatSummary>, DbError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "WITH activity AS (
               SELECT id, 'in' AS direction, body, created_at AS ts,
                      COALESCE(thread_id, 1) AS tid
               FROM inbound_log
               UNION ALL
               SELECT id, 'out', body, created_at, COALESCE(thread_id, 1)
               FROM outbound_log
             ),
             ranked AS (
               SELECT direction, body, ts, tid,
                      ROW_NUMBER() OVER (
                        PARTITION BY tid
                        ORDER BY ts DESC, id DESC, direction DESC
                      ) AS row_num
               FROM activity
             )
             SELECT r.tid,
                    COALESCE(
                      t.title,
                      CASE
                        WHEN r.tid = 1 THEN 'General'
                        ELSE 'Thread ' || r.tid
                      END
                    ),
                    t.contact_id,
                    c.display_name,
                    t.default_e164,
                    r.ts,
                    r.body,
                    r.direction,
                    t.sort
             FROM ranked r
             LEFT JOIN topics t ON t.thread_id = r.tid
             LEFT JOIN contacts c ON c.id = t.contact_id
             WHERE r.row_num = 1
               AND (?1 IS NULL OR r.ts < ?1)
               AND (?2 IS NULL OR r.ts > ?2)
             ORDER BY r.ts DESC
             LIMIT ?3",
        )?;
        let chats = stmt
            .query_map(rusqlite::params![before, after, limit], |row| {
                Ok(ChatSummary {
                    thread_id: row.get(0)?,
                    title: row.get(1)?,
                    contact_id: row.get(2)?,
                    display_name: row.get(3)?,
                    default_e164: row.get(4)?,
                    last_message_at: row.get(5)?,
                    last_message_preview: row.get(6)?,
                    last_message_direction: row.get(7)?,
                    sort: row.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(chats)
    }

    pub fn messages_for_thread(
        &self,
        thread_id: i32,
        limit: i64,
        before: Option<&str>,
        after: Option<&str>,
        filter: &MessageFilter,
    ) -> Result<Vec<MessageRow>, DbError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(&format!(
            "{MESSAGE_UNION}
             WHERE tid = ?1
               AND (?2 IS NULL OR ts < ?2)
               AND (?3 IS NULL OR ts > ?3)
               AND (?5 IS NULL OR category = ?5)
               AND (?6 IS NULL OR EXISTS (
                     SELECT 1 FROM json_each(tags) WHERE value = ?6))
             ORDER BY ts DESC, id DESC
             LIMIT ?4"
        ))?;
        let messages = stmt
            .query_map(
                rusqlite::params![thread_id, before, after, limit, filter.category, filter.tag],
                Self::message_from_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(messages)
    }

    /// Messages recorded after `after`, oldest first, across every thread.
    ///
    /// Used by realtime subscribers that reconnect with a cursor and need the
    /// messages they missed while they were away.
    pub fn messages_after(&self, after: &str, limit: i64) -> Result<Vec<MessageRow>, DbError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(&format!(
            "{MESSAGE_UNION}
             WHERE ts > ?1
             ORDER BY ts ASC, id ASC
             LIMIT ?2"
        ))?;
        let messages = stmt
            .query_map(rusqlite::params![after, limit], Self::message_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(messages)
    }

    fn message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRow> {
        let id: i64 = row.get(0)?;
        let direction: String = row.get(1)?;
        let sms_ts: Option<String> = row.get(5)?;
        Ok(MessageRow {
            id: format!("{direction}:{id}"),
            direction,
            e164: row.get(2)?,
            body: row.get(3)?,
            timestamp: row.get(4)?,
            sms_ts: sms_ts.filter(|value| !value.is_empty()),
            status: row.get(6)?,
            thread_id: row.get(7)?,
            classification: Self::classification_from(row.get(8)?, row.get(9)?, row.get(10)?),
            otp_code: row.get(11)?,
        })
    }

    #[cfg(test)]
    pub fn inbound_thread_id(&self, path: &str) -> Result<Option<i32>, DbError> {
        let conn = self.conn()?;
        conn.query_row(
            "SELECT thread_id FROM inbound_log WHERE mm_path = ?1",
            [path],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    #[cfg(test)]
    pub(crate) fn insert_inbound_at(
        &self,
        path: &str,
        e164: &str,
        body: &str,
        created_at: &str,
        thread_id: Option<i32>,
    ) -> Result<(), DbError> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO inbound_log (mm_path, e164, body, created_at, thread_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![path, e164, body, created_at, thread_id],
        )?;
        Ok(())
    }

    pub fn today_sms_counts(&self, since_rfc3339: &str) -> Result<TodaySmsCounts, DbError> {
        let conn = self.conn()?;
        let inbound: i64 = conn.query_row(
            "SELECT COUNT(*) FROM inbound_log WHERE created_at >= ?1",
            rusqlite::params![since_rfc3339],
            |row| row.get(0),
        )?;
        let sent_ok: i64 = conn.query_row(
            "SELECT COUNT(*) FROM outbound_log WHERE created_at >= ?1 AND result = 'ok'",
            rusqlite::params![since_rfc3339],
            |row| row.get(0),
        )?;
        let sent_fail: i64 = conn.query_row(
            "SELECT COUNT(*) FROM outbound_log WHERE created_at >= ?1 AND result <> 'ok'",
            rusqlite::params![since_rfc3339],
            |row| row.get(0),
        )?;
        Ok(TodaySmsCounts {
            inbound: inbound as u32,
            sent_ok: sent_ok as u32,
            sent_fail: sent_fail as u32,
        })
    }

    pub fn last_inbound(&self) -> Result<Option<(String, String)>, DbError> {
        let conn = self.conn()?;
        conn.query_row(
            "SELECT e164, created_at FROM inbound_log ORDER BY created_at DESC, id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn last_outbound_ok(&self) -> Result<Option<(String, String)>, DbError> {
        let conn = self.conn()?;
        conn.query_row(
            "SELECT e164, created_at FROM outbound_log
             WHERE result = 'ok' ORDER BY created_at DESC, id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn last_outbound_fail_since(&self, since_rfc3339: &str) -> Result<Option<String>, DbError> {
        let conn = self.conn()?;
        conn.query_row(
            "SELECT result FROM outbound_log
             WHERE created_at >= ?1 AND result <> 'ok'
             ORDER BY created_at DESC, id DESC LIMIT 1",
            rusqlite::params![since_rfc3339],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_an_inbound_message_returns_the_recorded_row() {
        let db = Db::open_in_memory().unwrap();
        let row = db
            .record_inbound(
                "/sms/1",
                "+989121234567",
                "hello",
                None,
                "2026-10-20T10:00:00+00:00",
                9,
                None,
                None,
            )
            .unwrap();

        assert_eq!(row.direction, "in");
        assert_eq!(row.e164, "+989121234567");
        assert_eq!(row.body, "hello");
        assert_eq!(row.status, "ok");
        assert_eq!(row.thread_id, 9);
        assert_eq!(row.sms_ts.as_deref(), Some("2026-10-20T10:00:00+00:00"));
        assert!(row.id.starts_with("in:"), "id: {}", row.id);
        assert!(!row.timestamp.is_empty());
    }

    #[test]
    fn recording_an_outbound_message_returns_its_status() {
        let db = Db::open_in_memory().unwrap();
        let sent = db.record_outbound("+989121234567", "hi", "ok", 9).unwrap();
        let failed = db
            .record_outbound("+989121234567", "hi", "modem error", 9)
            .unwrap();

        assert_eq!(sent.direction, "out");
        assert_eq!(sent.status, "ok");
        assert_eq!(failed.status, "failed");
        assert!(sent.id.starts_with("out:"), "id: {}", sent.id);
    }

    #[test]
    fn a_recorded_message_matches_what_the_api_returns() {
        let db = Db::open_in_memory().unwrap();
        let row = db
            .record_inbound(
                "/sms/1",
                "+989121234567",
                "hello",
                None,
                "2026-10-20T10:00:00+00:00",
                9,
                None,
                None,
            )
            .unwrap();

        let rows = db
            .messages_for_thread(9, 10, None, None, &MessageFilter::default())
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, row.id);
        assert_eq!(rows[0].body, "hello");
        assert_eq!(rows[0].thread_id, 9);
    }

    #[test]
    fn find_contact_by_e164_first_match_and_flag_ambiguity() {
        let db = Db::open_in_memory().unwrap();
        let a = db.upsert_contact("people/a", "Ali").unwrap();
        let b = db.upsert_contact("people/b", "Ali 2").unwrap();
        db.replace_contact_numbers(a, &["+989111111111".into()])
            .unwrap();
        db.replace_contact_numbers(b, &["+989111111111".into()])
            .unwrap();
        let c = db.find_contact_by_e164("+989111111111").unwrap().unwrap();
        assert_eq!(c.id, a);
        assert!(c.ambiguous);
    }

    #[test]
    fn topic_roundtrip_and_default() {
        let db = Db::open_in_memory().unwrap();
        let contact_id = db.upsert_contact("people/a", "Ali").unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: Some(contact_id),
            default_e164: None,
            title: "Ali (1111)".into(),
            ignored: false,
            sort: None,
        })
        .unwrap();
        db.set_default_number(42, "+989111111111").unwrap();
        let t = db.get_topic_by_thread(42).unwrap().unwrap();
        assert_eq!(t.thread_id, 42);
        assert_eq!(t.contact_id, Some(contact_id));
        assert_eq!(t.default_e164.as_deref(), Some("+989111111111"));
        assert_eq!(t.title, "Ali (1111)");
        assert!(!t.ignored);
    }

    #[test]
    fn ignore_number_persists() {
        let db = Db::open_in_memory().unwrap();
        assert!(!db.is_ignored("+98912").unwrap());
        db.ignore_number("+98912").unwrap();
        assert!(db.is_ignored("+98912").unwrap());
        assert!(!db.is_ignored("+98913").unwrap());
    }

    #[test]
    fn inbound_path_dedup() {
        let db = Db::open_in_memory().unwrap();
        assert!(!db.seen_sms_path("/sms/1").unwrap());
        db.record_inbound("/sms/1", "+98912", "hi", None, "", 1, None, None)
            .unwrap();
        assert!(db.seen_sms_path("/sms/1").unwrap());
        // Path alone must never mark a message seen: ModemManager reuses
        // low path numbers after every restart, so a path match may refer
        // to a completely different message seen weeks earlier.
        assert!(!db.seen_sms("/sms/1", "+98912", "hi", "").unwrap());
    }

    #[test]
    fn seen_sms_path_reuse_after_mm_restart_is_not_seen() {
        let db = Db::open_in_memory().unwrap();
        db.record_inbound(
            "/org/freedesktop/ModemManager1/SMS/17",
            "+989999987641",
            "old promo from weeks ago",
            None,
            "2026-08-20T15:42:30+00:00",
            1,
            None,
            None,
        )
        .unwrap();
        // After a host reboot ModemManager restarts numbering at SMS/0 and
        // the new message lands on a path that already exists in the log.
        assert!(!db
            .seen_sms(
                "/org/freedesktop/ModemManager1/SMS/17",
                "+989120000000",
                "fresh otp 1234",
                "2026-09-08T08:00:00+00:00",
            )
            .unwrap());
    }

    #[test]
    fn seen_sms_same_content_different_path() {
        let db = Db::open_in_memory().unwrap();
        db.record_inbound(
            "/sms/1",
            "+98912",
            "hi",
            None,
            "2026-08-19T12:00:00+00:00",
            1,
            None,
            None,
        )
        .unwrap();
        assert!(db
            .seen_sms("/sms/2", "+98912", "hi", "2026-08-19T12:00:00+00:00")
            .unwrap());
        assert!(!db
            .seen_sms("/sms/2", "+98912", "other", "2026-08-19T12:00:00+00:00")
            .unwrap());
    }

    #[test]
    fn seen_sms_empty_ts_is_never_seen() {
        let db = Db::open_in_memory().unwrap();
        db.record_inbound("/sms/1", "+98912", "hi", None, "", 1, None, None)
            .unwrap();
        assert!(!db.seen_sms("/sms/1", "+98912", "hi", "").unwrap());
        assert!(!db.seen_sms("/sms/2", "+98912", "hi", "").unwrap());
    }

    #[test]
    fn search_contacts_matches_name() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_contact("people/a", "Ali Reza").unwrap();
        let hits = db.search_contacts("ali").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].display_name, "Ali Reza");
    }

    #[test]
    fn search_contacts_unavailable_when_flag_cleared() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_contact("people/a", "Ali").unwrap();
        db.set_contacts_available(false);
        assert!(matches!(
            db.search_contacts("ali"),
            Err(DbError::ContactsUnavailable)
        ));
        db.set_contacts_available(true);
        assert_eq!(db.search_contacts("ali").unwrap().len(), 1);
    }

    fn insert_in(db: &Db, path: &str, e164: &str, created_at: &str) {
        db.conn()
            .unwrap()
            .execute(
                "INSERT INTO inbound_log (mm_path, e164, body, created_at) VALUES (?1, ?2, 'x', ?3)",
                rusqlite::params![path, e164, created_at],
            )
            .unwrap();
    }

    fn insert_out(db: &Db, e164: &str, result: &str, created_at: &str) {
        db.conn()
            .unwrap()
            .execute(
                "INSERT INTO outbound_log (e164, body, result, created_at) VALUES (?1, 'x', ?2, ?3)",
                rusqlite::params![e164, result, created_at],
            )
            .unwrap();
    }

    #[test]
    fn today_counts_split_on_since() {
        let db = Db::open_in_memory().unwrap();
        let midnight = "2026-08-19T00:00:00+00:00";
        insert_in(&db, "/a", "+989111111111", "2026-08-18T23:59:59+00:00");
        insert_in(&db, "/b", "+989111111112", "2026-08-19T00:00:00+00:00");
        insert_out(&db, "+989111111113", "ok", "2026-08-18T23:59:59+00:00");
        insert_out(&db, "+989111111114", "ok", "2026-08-19T01:00:00+00:00");
        insert_out(
            &db,
            "+989111111115",
            "modem error: timeout",
            "2026-08-19T02:00:00+00:00",
        );
        insert_out(&db, "+989111111116", "ok", "2026-08-19T03:00:00+00:00");
        let c = db.today_sms_counts(midnight).unwrap();
        assert_eq!(c.inbound, 1);
        assert_eq!(c.sent_ok, 2);
        assert_eq!(c.sent_fail, 1);
    }

    #[test]
    fn last_outbound_ok_skips_failures() {
        let db = Db::open_in_memory().unwrap();
        insert_out(&db, "+989111111111", "ok", "2026-08-19T01:00:00+00:00");
        insert_out(
            &db,
            "+989111111122",
            "modem error: x",
            "2026-08-19T02:00:00+00:00",
        );
        let last = db.last_outbound_ok().unwrap().unwrap();
        assert_eq!(last.0, "+989111111111");
        let fail = db
            .last_outbound_fail_since("2026-08-19T00:00:00+00:00")
            .unwrap()
            .unwrap();
        assert_eq!(fail, "modem error: x");
        assert!(db
            .last_outbound_fail_since("2026-08-19T03:00:00+00:00")
            .unwrap()
            .is_none());
    }

    #[test]
    fn last_inbound_is_latest() {
        let db = Db::open_in_memory().unwrap();
        insert_in(&db, "/a", "+989111111111", "2026-08-19T01:00:00+00:00");
        insert_in(&db, "/b", "+989111111122", "2026-08-19T02:00:00+00:00");
        let last = db.last_inbound().unwrap().unwrap();
        assert_eq!(last.0, "+989111111122");
        assert_eq!(last.1, "2026-08-19T02:00:00+00:00");
    }

    #[test]
    fn contacts_available_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.contacts_available());
        db.set_contacts_available(false);
        assert!(!db.contacts_available());
    }

    #[test]
    fn record_inbound_stores_thread_id() {
        let db = Db::open_in_memory().unwrap();
        db.record_inbound("/sms/1", "+98912", "hi", None, "", 42, None, None)
            .unwrap();
        let conn = db.conn().unwrap();
        let tid: i32 = conn
            .query_row(
                "SELECT thread_id FROM inbound_log WHERE mm_path = ?1",
                ["/sms/1"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tid, 42);
    }

    #[test]
    fn record_inbound_recycled_path_replaces_stale_row() {
        let db = Db::open_in_memory().unwrap();
        // Weeks ago a message was logged on this ModemManager path.
        db.record_inbound(
            "/org/freedesktop/ModemManager1/SMS/27",
            "+989188086139",
            "هی bruh",
            None,
            "2026-08-20T22:45:37+03:30",
            1,
            None,
            None,
        )
        .unwrap();
        // After a ModemManager restart the path is recycled for a new,
        // unrelated message; recording it must succeed, not hit UNIQUE.
        db.record_inbound(
            "/org/freedesktop/ModemManager1/SMS/27",
            "+989900000000",
            "fresh promo",
            None,
            "2026-09-08T13:21:01+03:30",
            1,
            None,
            None,
        )
        .unwrap();
        assert!(db
            .seen_sms(
                "/org/freedesktop/ModemManager1/SMS/27",
                "+989900000000",
                "fresh promo",
                "2026-09-08T13:21:01+03:30",
            )
            .unwrap());
    }

    #[test]
    fn record_outbound_stores_thread_id() {
        let db = Db::open_in_memory().unwrap();
        db.record_outbound("+98912", "bye", "ok", 7).unwrap();
        let conn = db.conn().unwrap();
        let tid: i32 = conn
            .query_row("SELECT thread_id FROM outbound_log LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(tid, 7);
    }

    #[test]
    fn backfill_assigns_topic_and_leaves_unknown_null() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121111111".into()])
            .unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: Some(id),
            default_e164: Some("+989121111111".into()),
            title: "Ali".into(),
            ignored: false,
            sort: None,
        })
        .unwrap();

        // Insert pre-migration style rows: thread_id NULL via raw SQL
        {
            let conn = db.conn().unwrap();
            conn.execute(
                "INSERT INTO inbound_log (mm_path, e164, body, created_at, sms_ts, thread_id)
                 VALUES ('/a', '+989121111111', 'hi', '2026-08-01T00:00:00Z', '', NULL)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO outbound_log (e164, body, result, created_at, thread_id)
                 VALUES ('+989129999999', 'x', 'ok', '2026-08-01T00:00:00Z', NULL)",
                [],
            )
            .unwrap();
        }

        db.backfill_thread_ids().unwrap();

        let conn = db.conn().unwrap();
        let in_tid: Option<i32> = conn
            .query_row(
                "SELECT thread_id FROM inbound_log WHERE mm_path = '/a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(in_tid, Some(42));
        let out_tid: Option<i32> = conn
            .query_row("SELECT thread_id FROM outbound_log LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(out_tid, None); // unknown number stays NULL → COALESCE to General at query time
    }

    #[test]
    fn chats_with_activity_orders_and_buckets_general() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121111111".into()])
            .unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: Some(id),
            default_e164: Some("+989121111111".into()),
            title: "Ali (1111)".into(),
            ignored: false,
            sort: None,
        })
        .unwrap();
        db.record_inbound("/1", "+989129999999", "unknown", None, "", 1, None, None)
            .unwrap();
        db.record_outbound("+989121111111", "later", "ok", 42)
            .unwrap();
        {
            let conn = db.conn().unwrap();
            conn.execute(
                "UPDATE inbound_log SET created_at = '2026-08-20T08:00:00Z' WHERE mm_path = '/1'",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE outbound_log SET created_at = '2026-08-20T09:00:00Z' WHERE thread_id = 42",
                [],
            )
            .unwrap();
        }

        let chats = db.chats_with_activity(50, None, None).unwrap();
        assert!(chats.len() >= 2);
        assert_eq!(chats[0].thread_id, 42);
        assert!(chats.iter().any(|c| c.thread_id == 1));
    }

    #[test]
    fn messages_for_thread_unions_and_marks_failed() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: None,
            default_e164: Some("+98912".into()),
            title: "X".into(),
            ignored: false,
            sort: None,
        })
        .unwrap();
        db.record_inbound(
            "/m1",
            "+98912",
            "hi",
            None,
            "2026-08-20T08:00:00Z",
            42,
            None,
            None,
        )
        .unwrap();
        db.record_outbound("+98912", "nope", "modem down", 42)
            .unwrap();

        let msgs = db
            .messages_for_thread(42, 50, None, None, &MessageFilter::default())
            .unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(msgs.iter().any(|m| m.direction == "in" && m.status == "ok"));
        assert!(msgs
            .iter()
            .any(|m| m.direction == "out" && m.status == "failed" && m.id.starts_with("out:")));
    }

    #[test]
    fn pending_forward_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        db.set_pending_forward(1, PendingForwardMode::Number, 99, 7)
            .unwrap();
        let p = db.get_pending_forward(1).unwrap().unwrap();
        assert!(matches!(p.mode, PendingForwardMode::Number));
        assert_eq!(p.edit_message_id, 7);
        let taken = db.take_pending_forward(1).unwrap().unwrap();
        assert!(db.get_pending_forward(1).unwrap().is_none());
        assert_eq!(taken.edit_chat_id, 99);
    }

    #[test]
    fn expired_pending_forward_is_cleared_and_ignored() {
        let db = Db::open_in_memory().unwrap();
        db.set_pending_forward(1, PendingForwardMode::Number, 99, 7)
            .unwrap();
        db.conn()
            .unwrap()
            .execute(
                "UPDATE pending_forward SET created_at = '2020-01-01T00:00:00Z'
                 WHERE thread_id = 1",
                [],
            )
            .unwrap();

        assert!(db.take_pending_forward(1).unwrap().is_none());
        assert!(db.get_pending_forward(1).unwrap().is_none());
    }

    #[test]
    fn messages_cursor_before_excludes_boundary() {
        let db = Db::open_in_memory().unwrap();
        {
            let conn = db.conn().unwrap();
            conn.execute(
                "INSERT INTO inbound_log (mm_path, e164, body, created_at, sms_ts, thread_id)
                 VALUES ('/a', '+1', 'old', '2026-08-01T00:00:00Z', '', 42)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO inbound_log (mm_path, e164, body, created_at, sms_ts, thread_id)
                 VALUES ('/b', '+1', 'new', '2026-08-02T00:00:00Z', '', 42)",
                [],
            )
            .unwrap();
        }
        let page = db
            .messages_for_thread(
                42,
                10,
                Some("2026-08-02T00:00:00Z"),
                None,
                &MessageFilter::default(),
            )
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].body, "old");
    }

    fn in_id(row: &MessageRow) -> i64 {
        row.id.trim_start_matches("in:").parse().unwrap()
    }

    fn stored(category: &str, confidence: f64, tags: &[&str]) -> StoredClassification {
        StoredClassification {
            public: Classification {
                category: category.into(),
                confidence,
                tags: tags.iter().map(|t| t.to_string()).collect(),
            },
            scores: serde_json::json!({ category: confidence, "is_scam": tags.contains(&"scam") }),
        }
    }

    fn classify_status(db: &Db, id: i64) -> Option<String> {
        let conn = db.conn().unwrap();
        conn.query_row(
            "SELECT classify_status FROM inbound_log WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn inbound_row_carries_classification_and_otp_code() {
        let db = Db::open_in_memory().unwrap();
        let c = stored("promotional", 0.61, &["scam"]);
        let recorded = db
            .record_inbound(
                "/sms/1",
                "+98912",
                "code 402332",
                None,
                "2026-10-20T10:00:00+00:00",
                1,
                Some("402332"),
                Some(&c),
            )
            .unwrap();
        assert_eq!(recorded.classification, Some(c.public.clone()));
        assert_eq!(recorded.otp_code.as_deref(), Some("402332"));

        let rows = db
            .messages_for_thread(1, 10, None, None, &MessageFilter::default())
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].classification,
            Some(Classification {
                category: "promotional".into(),
                confidence: 0.61,
                tags: vec!["scam".into()],
            })
        );
        assert_eq!(rows[0].otp_code.as_deref(), Some("402332"));
        assert_eq!(
            classify_status(&db, in_id(&recorded)).as_deref(),
            Some("done")
        );
    }

    #[test]
    fn unclassified_inbound_is_pending() {
        let db = Db::open_in_memory().unwrap();
        let row = db
            .record_inbound("/sms/1", "+98912", "hi", None, "", 1, None, None)
            .unwrap();
        assert_eq!(row.classification, None);
        assert_eq!(
            classify_status(&db, in_id(&row)).as_deref(),
            Some("pending")
        );
        let pending = db
            .pending_classification(None, &["pending"], None, 10)
            .unwrap();
        assert_eq!(
            pending,
            vec![(in_id(&row), "+98912".to_string(), "hi".to_string())]
        );
    }

    #[test]
    fn set_classification_and_otp_code_update_the_row() {
        let db = Db::open_in_memory().unwrap();
        let row = db
            .record_inbound("/sms/1", "+98912", "hi", Some(7), "", 1, None, None)
            .unwrap();
        let id = in_id(&row);
        let c = stored("transactional", 0.9, &[]);
        let updated = db.set_classification(id, &c).unwrap().unwrap();
        assert_eq!(updated.classification, Some(c.public));
        assert_eq!(classify_status(&db, id).as_deref(), Some("done"));
        let updated = db.set_otp_code(id, "1234").unwrap().unwrap();
        assert_eq!(updated.otp_code.as_deref(), Some("1234"));
        let (by_id, tg_msg) = db.inbound_by_id(id).unwrap().unwrap();
        assert_eq!(by_id.id, row.id);
        assert_eq!(tg_msg, Some(7));
        assert!(db
            .set_classification(999, &stored("a", 0.1, &[]))
            .unwrap()
            .is_none());
        assert!(db.inbound_by_id(999).unwrap().is_none());
    }

    #[test]
    fn outbound_rows_have_no_classification() {
        let db = Db::open_in_memory().unwrap();
        let sent = db.record_outbound("+98912", "hi", "ok", 1).unwrap();
        assert_eq!(sent.classification, None);
        assert_eq!(sent.otp_code, None);
        let rows = db
            .messages_for_thread(1, 10, None, None, &MessageFilter::default())
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].classification, None);
        assert_eq!(rows[0].otp_code, None);
        let after = db.messages_after("1970-01-01T00:00:00Z", 10).unwrap();
        assert_eq!(after[0].classification, None);
    }

    #[test]
    fn filter_by_category_and_tag() {
        let db = Db::open_in_memory().unwrap();
        let rows = [
            ("/1", stored("promotional", 0.7, &[])),
            ("/2", stored("transactional", 0.8, &[])),
            ("/3", stored("promotional", 0.6, &["scam"])),
        ];
        for (path, c) in &rows {
            db.record_inbound(path, "+98912", path, None, "", 1, None, Some(c))
                .unwrap();
        }
        db.record_outbound("+98912", "reply", "ok", 1).unwrap();

        let promo = MessageFilter {
            category: Some("promotional".into()),
            tag: None,
        };
        assert_eq!(
            db.messages_for_thread(1, 10, None, None, &promo)
                .unwrap()
                .len(),
            2
        );
        let scam = MessageFilter {
            category: None,
            tag: Some("scam".into()),
        };
        let found = db.messages_for_thread(1, 10, None, None, &scam).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].body, "/3");
        let all = db
            .messages_for_thread(1, 10, None, None, &MessageFilter::default())
            .unwrap();
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn failure_counts_up_to_failed() {
        let db = Db::open_in_memory().unwrap();
        let row = db
            .record_inbound("/sms/1", "+98912", "hi", None, "", 1, None, None)
            .unwrap();
        let id = in_id(&row);
        for _ in 0..4 {
            db.record_classify_failure(id, 5).unwrap();
        }
        assert_eq!(classify_status(&db, id).as_deref(), Some("pending"));
        db.record_classify_failure(id, 5).unwrap();
        assert_eq!(classify_status(&db, id).as_deref(), Some("failed"));
        assert!(db
            .pending_classification(None, &["pending"], Some(5), 10)
            .unwrap()
            .is_empty());
        let failed = db
            .pending_classification(None, &["failed"], None, 10)
            .unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, id);
    }

    #[test]
    fn pending_classification_filters_and_orders() {
        let db = Db::open_in_memory().unwrap();
        db.insert_inbound_at(
            "/old",
            "+1",
            "legacy null status",
            "2026-08-01T00:00:00Z",
            Some(1),
        )
        .unwrap();
        db.insert_inbound_at("/new", "+2", "newer", "2026-09-01T00:00:00Z", Some(1))
            .unwrap();
        let all = db
            .pending_classification(None, &["pending"], None, 10)
            .unwrap();
        assert_eq!(
            all.iter().map(|r| r.2.as_str()).collect::<Vec<_>>(),
            ["legacy null status", "newer"]
        );
        let recent = db
            .pending_classification(Some("2026-08-15T00:00:00Z"), &["pending"], None, 10)
            .unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].2, "newer");
        assert_eq!(
            db.pending_classification(None, &["pending"], None, 1)
                .unwrap()
                .len(),
            1
        );
        assert!(db
            .pending_classification(None, &[], None, 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn adding_same_pattern_twice_returns_existing() {
        let db = Db::open_in_memory().unwrap();
        let (first, created) = db.add_otp_pattern(Some("bank"), "code {code}").unwrap();
        assert!(created);
        assert_eq!(first.name, "bank");
        let (second, created) = db.add_otp_pattern(Some("other"), "code {code}").unwrap();
        assert!(!created);
        assert_eq!(second.id, first.id);
        assert_eq!(second.name, "bank");
        assert_eq!(db.otp_patterns().unwrap().len(), 1);
    }

    #[test]
    fn otp_pattern_default_name_and_delete() {
        let db = Db::open_in_memory().unwrap();
        let (row, _) = db.add_otp_pattern(None, "p {code}").unwrap();
        assert_eq!(row.name, format!("pattern-{}", row.id));
        assert!(!row.created_at.is_empty());
        assert!(db.delete_otp_pattern(row.id).unwrap());
        assert!(!db.delete_otp_pattern(row.id).unwrap());
        assert!(db.otp_patterns().unwrap().is_empty());
    }

    #[test]
    fn inbound_found_by_tg_msg() {
        let db = Db::open_in_memory().unwrap();
        let row = db
            .record_inbound("/sms/1", "+98912", "hi", Some(55), "", 3, None, None)
            .unwrap();
        let found = db.inbound_by_tg_msg(55).unwrap().unwrap();
        assert_eq!(found.id, row.id);
        assert_eq!(found.thread_id, 3);
    }

    #[test]
    fn missing_tg_msg_returns_none() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.inbound_by_tg_msg(55).unwrap().is_none());
    }

    #[test]
    fn sorted_topic_roundtrip_and_chat_summary_sort() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.sorted_topic("otp").unwrap(), None);
        db.upsert_sorted_topic("otp", 50, "OTP").unwrap();
        assert_eq!(db.sorted_topic("otp").unwrap(), Some(50));
        db.upsert_sorted_topic("otp", 51, "OTP").unwrap();
        assert_eq!(db.sorted_topic("otp").unwrap(), Some(51));
        db.record_inbound("/sms/1", "+98912", "hi", None, "", 51, None, None)
            .unwrap();
        db.record_inbound("/sms/2", "+98913", "yo", None, "", 1, None, None)
            .unwrap();
        let chats = db.chats_with_activity(10, None, None).unwrap();
        let sorted = chats.iter().find(|c| c.thread_id == 51).unwrap();
        assert_eq!(sorted.sort.as_deref(), Some("otp"));
        assert_eq!(sorted.title, "OTP");
        let general = chats.iter().find(|c| c.thread_id == 1).unwrap();
        assert_eq!(general.sort, None);
    }

    const OLD_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS contacts (
  id INTEGER PRIMARY KEY,
  google_resource TEXT UNIQUE NOT NULL,
  display_name TEXT NOT NULL,
  last_synced TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS contact_numbers (
  contact_id INTEGER NOT NULL REFERENCES contacts(id),
  e164 TEXT NOT NULL,
  last_seen_incoming TEXT,
  UNIQUE(contact_id, e164)
);
CREATE INDEX IF NOT EXISTS idx_numbers_e164 ON contact_numbers(e164);
CREATE TABLE IF NOT EXISTS topics (
  thread_id INTEGER PRIMARY KEY,
  contact_id INTEGER REFERENCES contacts(id),
  default_e164 TEXT,
  title TEXT NOT NULL,
  ignored INTEGER NOT NULL DEFAULT 0,
  pending_outbound TEXT,
  pending_reply_to INTEGER
);
CREATE TABLE IF NOT EXISTS number_ignore (
  e164 TEXT PRIMARY KEY
);
CREATE TABLE IF NOT EXISTS inbound_log (
  id INTEGER PRIMARY KEY,
  mm_path TEXT UNIQUE NOT NULL,
  e164 TEXT NOT NULL,
  body TEXT NOT NULL,
  tg_msg INTEGER,
  created_at TEXT NOT NULL,
  sms_ts TEXT
);
CREATE TABLE IF NOT EXISTS outbound_log (
  id INTEGER PRIMARY KEY,
  e164 TEXT NOT NULL,
  body TEXT NOT NULL,
  result TEXT NOT NULL,
  created_at TEXT NOT NULL
);
";

    #[test]
    fn existing_database_migrates() {
        let path = std::env::temp_dir().join(format!(
            "telesms-migrate-{}-{}.db",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(OLD_SCHEMA).unwrap();
            conn.execute(
                "INSERT INTO inbound_log (mm_path, e164, body, created_at, sms_ts)
                 VALUES ('/old', '+98912', 'legacy', '2026-08-01T00:00:00Z', '')",
                [],
            )
            .unwrap();
        }

        let result = std::panic::catch_unwind(|| {
            let db = Db::open(&path).unwrap();
            let c = stored("promotional", 0.5, &[]);
            db.record_inbound("/new", "+98912", "fresh", None, "", 1, Some("99"), Some(&c))
                .unwrap();
            let rows = db
                .messages_for_thread(1, 10, None, None, &MessageFilter::default())
                .unwrap();
            assert_eq!(rows.len(), 2);
            let legacy = rows.iter().find(|r| r.body == "legacy").unwrap();
            assert_eq!(legacy.classification, None);
            assert_eq!(legacy.otp_code, None);
            let fresh = rows.iter().find(|r| r.body == "fresh").unwrap();
            assert_eq!(fresh.classification, Some(c.public));
            // Rows from before the feature count as pending for backfill.
            let pending = db
                .pending_classification(None, &["pending"], None, 10)
                .unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].2, "legacy");
            db.add_otp_pattern(None, "x {code}").unwrap();
            drop(db);
            // Reopening an already-migrated database must not fail.
            Db::open(&path).unwrap();
        });
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        result.unwrap();
    }
}
