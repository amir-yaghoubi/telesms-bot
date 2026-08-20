# Chat History API Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `GET /api/v1/chats` and `GET /api/v1/chats/{thread_id}/messages` that serve local SMS history attributed to Telegram forum threads.

**Architecture:** Persist `thread_id` on every inbound/outbound SMS log row (Approach B). Startup backfill fills older `NULL` rows. Read-only `actions` + axum GET handlers query SQLite; no Telegram scrape.

**Tech Stack:** Rust 2021, Tokio, axum 0.8, rusqlite, serde/serde_json, chrono (RFC3339 cursors). Existing `Db`, `actions::ActionError`, `http::router` patterns.

**Spec:** `docs/superpowers/specs/2026-08-20-chat-history-api-design.md`

## Global Constraints

- Auth: `X-Api-Key` (existing middleware). Same error envelope as other `/api/v1` routes.
- Source of truth: SQLite SMS logs only (not Telegram history).
- Chat list: topics with SMS activity only; unknown/ignored SMS bucket into General (`thread_id = 1`).
- Messages include inbound + outbound ok + outbound failed.
- Pagination: `limit` default 50, max 100; `before` / `after` exclusive ISO-8601; both set requires `before > after` else `400 validation`.
- Path `thread_id = 1` always General; other ids require a `topics` row or `404`.
- Optional query `number` / `contact_id` on messages: consistency check only; mismatch → `409 identity_conflict`.
- `unread_count` always JSON `null` in v1.
- `docs/superpowers/` is gitignored; force-add only files you intend to version.
- Follow commit style: `feat:` / `fix:` / `docs:` with a short why.
- After every `cargo test` step, all previously passing tests must still pass.

---

## File map

| Path | Responsibility |
|---|---|
| `src/db.rs` | `ALTER` `thread_id`; `record_inbound` / `record_outbound` take `thread_id`; `backfill_thread_ids`; `chats_with_activity`; `messages_for_thread`; types `ChatSummary`, `MessageRow` |
| `src/app.rs` | Pass resolved `thread_id` into record calls; extend `Delivered` to carry thread; stale path resolves without creating topics |
| `src/actions.rs` | `list_chats`, `list_messages`, pagination parse helpers, response DTOs |
| `src/http.rs` | GET routes + handlers + axum tests |
| `docs/openapi.yaml` | Document both routes + schemas |
| `README.md` | Add two rows to HTTP API table |

Do not split `actions.rs` unless this work pushes it past ~1200 lines; prefer keeping history helpers next to existing action code.

---

### Task 1: Persist `thread_id` on SMS logs

**Files:**
- Modify: `src/db.rs` (`from_conn` migrations, `record_inbound`, `record_outbound`, test helpers, unit tests)
- Modify: `src/app.rs` (`handle_incoming`, `deliver_incoming`, `send_and_ack`, related tests)

**Interfaces:**
- Consumes: existing `route_inbound`, `GENERAL_THREAD`, `send_and_ack(..., thread_id, ...)`
- Produces:
  - `Db::record_inbound(..., thread_id: i32)`
  - `Db::record_outbound(e164, text, result, thread_id: i32)`
  - `Delivered::Normalized { e164: String, thread_id: i32 }` and `Delivered::Raw { thread_id: i32 }`

- [ ] **Step 1: Write failing Db tests** in `src/db.rs` `mod tests`

```rust
#[test]
fn record_inbound_stores_thread_id() {
    let db = Db::open_in_memory().unwrap();
    db.record_inbound("/sms/1", "+98912", "hi", None, "", 42)
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
fn record_outbound_stores_thread_id() {
    let db = Db::open_in_memory().unwrap();
    db.record_outbound("+98912", "bye", "ok", 7).unwrap();
    let conn = db.conn().unwrap();
    let tid: i32 = conn
        .query_row("SELECT thread_id FROM outbound_log LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tid, 7);
}
```

Update every existing `record_inbound` / `record_outbound` call in `db.rs` tests to pass a `thread_id` (use `1` or a topic id as appropriate) so the suite compiles after the signature change — do that in Step 3 with the implementation.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib db::tests::record_inbound_stores_thread_id -- --nocapture`

Expected: compile error (`this function takes 5 arguments but 6 arguments were supplied`) or FAIL.

- [ ] **Step 3: Implement schema + record methods**

In `Db::from_conn`, after existing ALTERs:

```rust
let _ = conn.execute("ALTER TABLE inbound_log ADD COLUMN thread_id INTEGER", []);
let _ = conn.execute("ALTER TABLE outbound_log ADD COLUMN thread_id INTEGER", []);
```

Change inserts:

```rust
pub fn record_inbound(
    &self,
    path: &str,
    e164: &str,
    text: &str,
    tg_msg: Option<i32>,
    sms_ts: &str,
    thread_id: i32,
) -> Result<(), DbError> {
    let conn = self.conn()?;
    conn.execute(
        "INSERT INTO inbound_log (mm_path, e164, body, tg_msg, created_at, sms_ts, thread_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![path, e164, text, tg_msg, Self::now(), sms_ts, thread_id],
    )?;
    Ok(())
}

pub fn record_outbound(
    &self,
    e164: &str,
    text: &str,
    result: &str,
    thread_id: i32,
) -> Result<(), DbError> {
    let conn = self.conn()?;
    conn.execute(
        "INSERT INTO outbound_log (e164, body, result, created_at, thread_id)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![e164, text, result, Self::now(), thread_id],
    )?;
    Ok(())
}
```

If `insert_inbound_at` exists for tests, add an optional `thread_id: Option<i32>` column insert (or a sibling helper) so later history tests can plant timed rows with threads.

- [ ] **Step 4: Wire `app.rs` call sites**

1. Change `Delivered` to carry `thread_id`:

```rust
enum Delivered {
    Normalized { e164: String, thread_id: i32 },
    Raw { thread_id: i32 },
}
```

2. In `deliver_incoming`, set `thread_id` from each `InboundDest` branch (`create` → created id, `ExistingTopic` → that id, `General` / unparseable Raw → `GENERAL_THREAD`). Return it in `Delivered`.

3. In `handle_incoming` success paths, pass that `thread_id` into `record_inbound`.

4. Stale skip path (no topic create): resolve without side effects:

```rust
let thread_id = match route_inbound(db, &id_e164)? {
    InboundDest::ExistingTopic { thread_id, .. } => thread_id,
    InboundDest::CreateContactTopic { .. } | InboundDest::General { .. } => GENERAL_THREAD,
};
db.record_inbound(&sms.path, &id_e164, &sms.text, None, &sms.timestamp, thread_id)?;
```

5. In `send_and_ack`, pass the existing `thread_id` argument into both `record_outbound` calls.

6. Fix all compile breaks in `app.rs` / `actions` tests that call `record_*` or match on `Delivered`.

- [ ] **Step 5: Run tests**

Run: `cargo test --lib db::tests::record_inbound_stores_thread_id db::tests::record_outbound_stores_thread_id -- --nocapture`

Expected: PASS

Run: `cargo test --lib -- --nocapture`

Expected: PASS (full lib)

- [ ] **Step 6: Commit**

```bash
git add src/db.rs src/app.rs
git commit -m "$(cat <<'EOF'
feat: store forum thread_id on SMS log rows

EOF
)"
```

---

### Task 2: Startup backfill for `NULL` thread_ids

**Files:**
- Modify: `src/db.rs` (add `backfill_thread_ids`, call from `from_conn` after ALTERs)
- Test: `src/db.rs` `mod tests`

**Interfaces:**
- Consumes: `topics.default_e164`, `topics.contact_id` + `contact_numbers.e164`
- Produces: `Db::backfill_thread_ids(&self) -> Result<u64, DbError>` (rows updated; return value optional but useful in logs)

- [ ] **Step 1: Write failing test**

```rust
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
        .query_row("SELECT thread_id FROM outbound_log LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(out_tid, None); // unknown number stays NULL → COALESCE to General at query time
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib db::tests::backfill_assigns_topic_and_leaves_unknown_null -- --nocapture`

Expected: compile error (`no method named backfill_thread_ids`) or FAIL.

- [ ] **Step 3: Implement backfill**

Prefer two UPDATEs per table (default match, then contact_numbers match). Example for inbound (mirror for outbound):

```rust
pub fn backfill_thread_ids(&self) -> Result<(), DbError> {
    let conn = self.conn()?;
    conn.execute(
        "UPDATE inbound_log
         SET thread_id = (
           SELECT t.thread_id FROM topics t
           WHERE t.default_e164 = inbound_log.e164
           ORDER BY t.thread_id ASC LIMIT 1
         )
         WHERE thread_id IS NULL",
        [],
    )?;
    conn.execute(
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
    )?;
    // same two UPDATEs for outbound_log
    Ok(())
}
```

Call `self.backfill_thread_ids()?` at the end of `from_conn` (after ALTERs). Ignore individual UPDATE “no change” — both are fine.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib db::tests::backfill_assigns_topic_and_leaves_unknown_null -- --nocapture`

Expected: PASS

Run: `cargo test --lib -- --nocapture`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/db.rs
git commit -m "$(cat <<'EOF'
feat: backfill SMS log thread_id from topics

EOF
)"
```

---

### Task 3: Db query helpers for chats and messages

**Files:**
- Modify: `src/db.rs` (structs + `chats_with_activity` + `messages_for_thread` + tests)

**Interfaces:**
- Consumes: `COALESCE(thread_id, 1)`, `topics` metadata
- Produces:

```rust
#[derive(Clone, Debug)]
pub struct ChatSummary {
    pub thread_id: i32,
    pub title: String,
    pub contact_id: Option<i64>,
    pub display_name: Option<String>,
    pub default_e164: Option<String>,
    pub last_message_at: String,
    pub last_message_preview: String,
    pub last_message_direction: String, // "in" | "out"
}

#[derive(Clone, Debug)]
pub struct MessageRow {
    pub id: String,          // "in:88" / "out:55"
    pub direction: String,   // "in" | "out"
    pub e164: String,
    pub body: String,
    pub timestamp: String,   // created_at
    pub sms_ts: Option<String>,
    pub status: String,      // "ok" | "failed"
}

impl Db {
    pub fn chats_with_activity(
        &self,
        limit: i64,
        before: Option<&str>,
        after: Option<&str>,
    ) -> Result<Vec<ChatSummary>, DbError>;

    pub fn messages_for_thread(
        &self,
        thread_id: i32,
        limit: i64,
        before: Option<&str>,
        after: Option<&str>,
    ) -> Result<Vec<MessageRow>, DbError>;
}
```

- [ ] **Step 1: Write failing tests**

```rust
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
    })
    .unwrap();
    db.record_inbound("/1", "+989129999999", "unknown", None, "", 1)
        .unwrap();
    // Force older/newer timestamps via SQL if now() collides — or sleep 1ms and record topic SMS second
    db.record_outbound("+989121111111", "later", "ok", 42)
        .unwrap();

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
    })
    .unwrap();
    db.record_inbound("/m1", "+98912", "hi", None, "2026-08-20T08:00:00Z", 42)
        .unwrap();
    db.record_outbound("+98912", "nope", "modem down", 42)
        .unwrap();

    let msgs = db.messages_for_thread(42, 50, None, None).unwrap();
    assert_eq!(msgs.len(), 2);
    assert!(msgs.iter().any(|m| m.direction == "in" && m.status == "ok"));
    assert!(msgs
        .iter()
        .any(|m| m.direction == "out" && m.status == "failed" && m.id.starts_with("out:")));
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
        .messages_for_thread(42, 10, Some("2026-08-02T00:00:00Z"), None)
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].body, "old");
}
```

Also add a topic row for thread 42 in the cursor test if your 404 rules are enforced at action layer only (Db helper may allow any thread_id).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib db::tests::chats_with_activity_orders_and_buckets_general -- --nocapture`

Expected: compile error or FAIL.

- [ ] **Step 3: Implement queries**

**Messages** (union + filters):

```sql
SELECT * FROM (
  SELECT id, 'in' AS direction, e164, body, created_at AS ts, sms_ts,
         'ok' AS status, COALESCE(thread_id, 1) AS tid
  FROM inbound_log
  UNION ALL
  SELECT id, 'out', e164, body, created_at, NULL,
         CASE WHEN result = 'ok' THEN 'ok' ELSE 'failed' END,
         COALESCE(thread_id, 1)
  FROM outbound_log
)
WHERE tid = ?1
  AND (?2 IS NULL OR ts < ?2)   -- before
  AND (?3 IS NULL OR ts > ?3)   -- after
ORDER BY ts DESC, id DESC
LIMIT ?4
```

Map `id` column to `format!("{direction}:{id}")`. Empty `sms_ts` → `None`.

**Chats:** build from the same union, group by `tid`, take latest row per thread (SQLite window or “max ts then join”). Join `topics` + `contacts` for metadata. For `tid = 1` with no topic row: `title = "General"`, null contact fields. Apply `before`/`after` on `last_message_at`, `ORDER BY last_message_at DESC`, `LIMIT`.

Preview: truncate body to a reasonable length (e.g. 120 chars) if desired; full body is fine for personal SMS.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib db::tests::chats_with_activity_orders_and_buckets_general db::tests::messages_for_thread_unions_and_marks_failed db::tests::messages_cursor_before_excludes_boundary -- --nocapture`

Expected: PASS

Run: `cargo test --lib -- --nocapture`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/db.rs
git commit -m "$(cat <<'EOF'
feat: query chat inbox and thread SMS timelines

EOF
)"
```

---

### Task 4: Actions for list chats / messages

**Files:**
- Modify: `src/actions.rs` (DTOs, `parse_history_limit`, `parse_rfc3339_cursor`, `list_chats`, `list_messages`, unit tests)

**Interfaces:**
- Consumes: `Db::chats_with_activity`, `Db::messages_for_thread`, `Db::get_topic_by_thread`, `normalize_e164`, `Identity` / resolve rules for optional consistency
- Produces:

```rust
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatList {
    pub chats: Vec<ChatListItem>,
    pub next_before: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatListItem {
    pub thread_id: i32,
    pub title: String,
    pub contact_id: Option<i64>,
    pub display_name: Option<String>,
    pub default_e164: Option<String>,
    pub last_message_at: String,
    pub last_message_preview: String,
    pub last_message_direction: String,
    pub unread_count: Option<i64>, // always None / serialize as null
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MessageList {
    pub thread_id: i32,
    pub title: String,
    pub contact_id: Option<i64>,
    pub messages: Vec<MessageItem>,
    pub next_before: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MessageItem { /* same fields as MessageRow; Serialize */ }

pub fn list_chats(
    db: &Db,
    limit: Option<i64>,
    before: Option<&str>,
    after: Option<&str>,
) -> Result<ChatList, ActionError>;

pub fn list_messages(
    db: &Db,
    region: &str,
    thread_id: i32,
    limit: Option<i64>,
    before: Option<&str>,
    after: Option<&str>,
    number: Option<&str>,
    contact_id: Option<i64>,
) -> Result<MessageList, ActionError>;
```

- [ ] **Step 1: Write failing action tests**

```rust
#[test]
fn list_chats_empty_ok() {
    let db = Db::open_in_memory().unwrap();
    let out = list_chats(&db, None, None, None).unwrap();
    assert!(out.chats.is_empty());
    assert!(out.next_before.is_none());
}

#[test]
fn list_messages_unknown_thread_404() {
    let db = Db::open_in_memory().unwrap();
    let err = list_messages(&db, "IR", 99, None, None, None, None, None).unwrap_err();
    assert!(matches!(err, ActionError::NotFound(_)));
}

#[test]
fn list_messages_general_without_topic_ok() {
    let db = Db::open_in_memory().unwrap();
    db.record_inbound("/g", "+98912", "x", None, "", GENERAL_THREAD)
        .unwrap();
    let out = list_messages(&db, "IR", GENERAL_THREAD, None, None, None, None, None).unwrap();
    assert_eq!(out.thread_id, 1);
    assert_eq!(out.title, "General");
    assert_eq!(out.messages.len(), 1);
}

#[test]
fn list_messages_identity_conflict() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_contact("people/a", "Ali").unwrap();
    db.upsert_topic(&Topic {
        thread_id: 42,
        contact_id: Some(id),
        default_e164: Some("+989121111111".into()),
        title: "Ali".into(),
        ignored: false,
    })
    .unwrap();
    let err = list_messages(
        &db,
        "IR",
        42,
        None,
        None,
        None,
        None,
        Some(id + 1),
    )
    .unwrap_err();
    assert!(matches!(err, ActionError::IdentityConflict));
}

#[test]
fn bad_limit_validation() {
    let db = Db::open_in_memory().unwrap();
    let err = list_chats(&db, Some(0), None, None).unwrap_err();
    assert!(matches!(err, ActionError::Validation(_)));
}

#[test]
fn before_not_after_after_validation() {
    let db = Db::open_in_memory().unwrap();
    let err = list_chats(
        &db,
        None,
        Some("2026-08-01T00:00:00Z"),
        Some("2026-08-02T00:00:00Z"),
    )
    .unwrap_err();
    assert!(matches!(err, ActionError::Validation(_)));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib actions::tests::list_chats_empty_ok -- --nocapture`

Expected: compile error or FAIL.

- [ ] **Step 3: Implement actions**

Shared helpers:

```rust
fn history_limit(limit: Option<i64>) -> Result<i64, ActionError> {
    let n = limit.unwrap_or(50);
    if n < 1 || n > 100 {
        return Err(ActionError::Validation("limit must be 1..=100".into()));
    }
    Ok(n)
}

fn parse_cursor(raw: Option<&str>) -> Result<Option<String>, ActionError> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    chrono::DateTime::parse_from_rfc3339(s)
        .map_err(|_| ActionError::Validation("invalid cursor timestamp".into()))?;
    Ok(Some(s.to_string()))
}

fn validate_cursors(before: &Option<String>, after: &Option<String>) -> Result<(), ActionError> {
    if let (Some(b), Some(a)) = (before, after) {
        let b = chrono::DateTime::parse_from_rfc3339(b).unwrap();
        let a = chrono::DateTime::parse_from_rfc3339(a).unwrap();
        if b <= a {
            return Err(ActionError::Validation(
                "before must be greater than after".into(),
            ));
        }
    }
    Ok(())
}
```

`list_chats`: validate → `db.chats_with_activity` → map to `ChatListItem` with `unread_count: None` → `next_before` = last item’s `last_message_at` if `chats.len() == limit as usize`, else `None`.

`list_messages`:
1. If `thread_id != GENERAL_THREAD` and `db.get_topic_by_thread(thread_id)?.is_none()` → `NotFound("unknown thread")`.
2. Optional consistency: if `contact_id`/`number` provided, resolve against topic (General: number alone is fine; contact_id on General → conflict unless you define otherwise — **rule:** any `contact_id` with General → `IdentityConflict`; for contact topics, `contact_id` must match topic; `number` must normalize and equal topic default **or** be in `contact_numbers` for that contact).
3. Fetch messages; set `title`/`contact_id` from topic or `"General"` / `None`.
4. `next_before` same page-full rule on last message `timestamp`.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib actions::tests::list_chats_empty_ok actions::tests::list_messages_unknown_thread_404 actions::tests::list_messages_general_without_topic_ok actions::tests::list_messages_identity_conflict actions::tests::bad_limit_validation actions::tests::before_not_after_after_validation -- --nocapture`

Expected: PASS

Run: `cargo test --lib -- --nocapture`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/actions.rs
git commit -m "$(cat <<'EOF'
feat: add list_chats and list_messages actions

EOF
)"
```

---

### Task 5: HTTP GET routes

**Files:**
- Modify: `src/http.rs` (router, handlers, tests)

**Interfaces:**
- Consumes: `actions::list_chats`, `actions::list_messages`
- Produces: routes
  - `GET /api/v1/chats`
  - `GET /api/v1/chats/{thread_id}/messages`

- [ ] **Step 1: Write failing HTTP tests** in `src/http.rs` `mod tests`

```rust
#[tokio::test]
async fn chats_requires_api_key() {
    let app = test_router("secret");
    let res = call(
        app,
        axum::http::Request::builder()
            .method("GET")
            .uri("/api/v1/chats")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn chats_empty_ok() {
    let app = test_router("secret");
    let res = call(
        app,
        axum::http::Request::builder()
            .method("GET")
            .uri("/api/v1/chats")
            .header("X-Api-Key", "secret")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let v = body_json(res).await;
    assert_eq!(v["chats"], json!([]));
}

#[tokio::test]
async fn messages_unknown_thread_404() {
    let app = test_router("secret");
    let res = call(
        app,
        axum::http::Request::builder()
            .method("GET")
            .uri("/api/v1/chats/99/messages")
            .header("X-Api-Key", "secret")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let v = body_json(res).await;
    assert_eq!(v["error"], "not_found");
}
```

Extend `test_router` seed if needed so message history happy-path can be covered in one more test (insert topic + inbound via `state.db`).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib http::tests::chats_empty_ok -- --nocapture`

Expected: `404` from nest router (route missing) or FAIL assert.

- [ ] **Step 3: Implement handlers**

```rust
use axum::extract::Path;
use axum::extract::Query;

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    limit: Option<i64>,
    before: Option<String>,
    after: Option<String>,
    number: Option<String>,
    contact_id: Option<i64>,
}

async fn chats_handler(
    State(state): State<HttpState>,
    Query(q): Query<HistoryQuery>,
) -> Response {
    match actions::list_chats(
        state.db.as_ref(),
        q.limit,
        q.before.as_deref(),
        q.after.as_deref(),
    ) {
        Ok(body) => (StatusCode::OK, Json(body)).into_response(),
        Err(err) => action_to_response(err).into_response(),
    }
}

async fn chat_messages_handler(
    State(state): State<HttpState>,
    Path(thread_id): Path<i32>,
    Query(q): Query<HistoryQuery>,
) -> Response {
    match actions::list_messages(
        state.db.as_ref(),
        &state.cfg.default_region,
        thread_id,
        q.limit,
        q.before.as_deref(),
        q.after.as_deref(),
        q.number.as_deref(),
        q.contact_id,
    ) {
        Ok(body) => (StatusCode::OK, Json(body)).into_response(),
        Err(err) => action_to_response(err).into_response(),
    }
}
```

Register:

```rust
.route("/chats", get(chats_handler))
.route("/chats/{thread_id}/messages", get(chat_messages_handler))
```

(axum 0.8 path syntax: `{thread_id}`.)

- [ ] **Step 4: Run tests**

Run: `cargo test --lib http::tests::chats_requires_api_key http::tests::chats_empty_ok http::tests::messages_unknown_thread_404 -- --nocapture`

Expected: PASS

Run: `cargo test --lib -- --nocapture`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/http.rs
git commit -m "$(cat <<'EOF'
feat: expose chat history over HTTP GET

EOF
)"
```

---

### Task 6: OpenAPI + README

**Files:**
- Modify: `docs/openapi.yaml`
- Modify: `README.md` (HTTP API table)

**Interfaces:**
- Consumes: response shapes from Task 4
- Produces: documented routes matching the running server

- [ ] **Step 1: Extend OpenAPI**

Add tag `History` (or reuse `Messaging`). Document:

- `GET /api/v1/chats` with query `limit`, `before`, `after`
- `GET /api/v1/chats/{thread_id}/messages` with query `limit`, `before`, `after`, `number`, `contact_id`

Schemas: `ChatList`, `ChatListItem`, `MessageList`, `MessageItem` matching Serialize fields. Include `401` / `400` / `404` / `409` / `500` where applicable.

Bump `info.version` to `1.1.0`.

- [ ] **Step 2: Update README table**

Add:

| `/api/v1/chats` | GET | Recent chats with SMS activity |
| `/api/v1/chats/{thread_id}/messages` | GET | SMS timeline for a forum thread |

Optionally link the design spec path under the existing HTTP API blurb.

- [ ] **Step 3: Sanity-check OpenAPI still loads** (optional)

Run: `./scripts/openapi-preview.sh` briefly, or `python -c "import yaml; yaml.safe_load(open('docs/openapi.yaml'))"` if PyYAML is available.

- [ ] **Step 4: Commit**

```bash
git add docs/openapi.yaml README.md
git commit -m "$(cat <<'EOF'
docs: document chat history HTTP endpoints

EOF
)"
```

---

## Spec coverage checklist

| Spec requirement | Task |
|---|---|
| `thread_id` columns + write path | Task 1 |
| Startup backfill + COALESCE General | Task 2 (+ query COALESCE in Task 3) |
| `GET /api/v1/chats` inbox | Tasks 3–5 |
| General bucket `thread_id=1` | Tasks 3–4 |
| `GET /api/v1/chats/{thread_id}/messages` | Tasks 3–5 |
| All SMS including failed outbound | Task 3 |
| Cursor pagination `limit`/`before`/`after` | Tasks 3–5 |
| Optional identity consistency / 409 | Task 4 |
| 404 unknown non-General thread | Task 4–5 |
| OpenAPI + README | Task 6 |
| No Telegram scrape / no unread | Non-goals — no task |

---

## Self-review notes (plan author)

- No TBD/placeholder steps; signatures named consistently (`list_chats` / `list_messages`, `ChatSummary` vs `ChatListItem` mapped in actions).
- Stale inbound path explicitly does not create topics (avoids side effects on skip).
- Cursor page-full rule for `next_before` is defined in Task 4 (only when `len == limit`).
