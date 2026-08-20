use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;

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
";

#[derive(Debug, Error)]
pub enum DbError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database lock poisoned")]
    Poisoned,
    #[error("contacts unavailable")]
    ContactsUnavailable,
}

pub struct Db {
    conn: Mutex<Connection>,
    contacts_available: AtomicBool,
}

pub struct Contact {
    pub id: i64,
    pub google_resource: String,
    pub display_name: String,
    pub numbers: Vec<String>,
    pub ambiguous: bool,
}

pub struct Topic {
    pub thread_id: i32,
    pub contact_id: Option<i64>,
    pub default_e164: Option<String>,
    pub title: String,
    pub ignored: bool,
}

pub struct TodaySmsCounts {
    pub inbound: u32,
    pub sent_ok: u32,
    pub sent_fail: u32,
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
        Ok(Db {
            conn: Mutex::new(conn),
            contacts_available: AtomicBool::new(true),
        })
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
            "SELECT thread_id, contact_id, default_e164, title, ignored
             FROM topics WHERE thread_id = ?1",
            rusqlite::params![thread_id],
        )
    }

    pub fn get_topic_by_contact(&self, contact_id: i64) -> Result<Option<Topic>, DbError> {
        self.get_topic(
            "SELECT thread_id, contact_id, default_e164, title, ignored
             FROM topics WHERE contact_id = ?1
             ORDER BY thread_id ASC
             LIMIT 1",
            rusqlite::params![contact_id],
        )
    }

    pub fn get_topic_by_e164(&self, e164: &str) -> Result<Option<Topic>, DbError> {
        self.get_topic(
            "SELECT thread_id, contact_id, default_e164, title, ignored
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

    /// D-Bus SMS paths change when ModemManager reloads the inbox.
    /// Number + body + modem timestamp is stable across those reloads.
    pub fn seen_sms(
        &self,
        path: &str,
        e164: &str,
        text: &str,
        sms_ts: &str,
    ) -> Result<bool, DbError> {
        if self.seen_sms_path(path)? {
            return Ok(true);
        }
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

    pub fn record_inbound(
        &self,
        path: &str,
        e164: &str,
        text: &str,
        tg_msg: Option<i32>,
        sms_ts: &str,
    ) -> Result<(), DbError> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO inbound_log (mm_path, e164, body, tg_msg, created_at, sms_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![path, e164, text, tg_msg, Self::now(), sms_ts],
        )?;
        Ok(())
    }

    pub fn record_outbound(&self, e164: &str, text: &str, result: &str) -> Result<(), DbError> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO outbound_log (e164, body, result, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![e164, text, result, Self::now()],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn insert_inbound_at(
        &self,
        path: &str,
        e164: &str,
        body: &str,
        created_at: &str,
    ) -> Result<(), DbError> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO inbound_log (mm_path, e164, body, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![path, e164, body, created_at],
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
        db.record_inbound("/sms/1", "+98912", "hi", None, "")
            .unwrap();
        assert!(db.seen_sms_path("/sms/1").unwrap());
        assert!(db.seen_sms("/sms/1", "+98912", "hi", "").unwrap());
    }

    #[test]
    fn seen_sms_same_content_different_path() {
        let db = Db::open_in_memory().unwrap();
        db.record_inbound("/sms/1", "+98912", "hi", None, "2026-08-19T12:00:00+00:00")
            .unwrap();
        assert!(db
            .seen_sms("/sms/2", "+98912", "hi", "2026-08-19T12:00:00+00:00")
            .unwrap());
        assert!(!db
            .seen_sms("/sms/2", "+98912", "other", "2026-08-19T12:00:00+00:00")
            .unwrap());
    }

    #[test]
    fn seen_sms_empty_ts_does_not_content_match() {
        let db = Db::open_in_memory().unwrap();
        db.record_inbound("/sms/1", "+98912", "hi", None, "")
            .unwrap();
        assert!(!db.seen_sms("/sms/2", "+98912", "hi", "").unwrap());
        assert!(db.seen_sms("/sms/1", "+98912", "hi", "").unwrap());
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
}
