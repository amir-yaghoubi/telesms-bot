# SMS Send Telegram Reactions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace SMS-send success text replies with emoji reactions (`📨` → `✅` / `❌`) on the owner’s Telegram message, while still replying with error text on failure.

**Architecture:** Extend `TelegramSink` with `react`; `send_and_ack` owns the lifecycle (pending → modem → outcome). `RealTg` calls `set_message_reaction`; `FakeTg` records reactions for tests. No `reply_to` keeps today’s text post fallback.

**Tech Stack:** Rust 2021, Tokio, teloxide 0.17 / teloxide-core `set_message_reaction` + `ReactionType::Emoji`.

**Spec:** `docs/superpowers/specs/2026-08-20-sms-send-reactions-design.md`

## Global Constraints

- With `reply_to`: react `📨` before send; success → react `✅` only (no success text); failure → react `❌` **and** text reply with error.
- Without `reply_to`: no reactions; success post `✅`; failure post error text.
- `📨` react failure before modem → `ActionError::TelegramFailed { sent: false }`; do not call modem.
- Success path, final `✅` react failure → `ActionError::TelegramFailed { sent: true }`.
- Modem failure: best-effort `❌` (+ error reply when `reply_to`); return `ModemFailed`; ignore ack errors on fail (same as today).
- Exact emoji strings: `📨`, `✅`, `❌`. If live Telegram rejects them (bot reaction allowlist), stop and ask before swapping.
- No DB / HTTP / OpenAPI changes.
- `docs/superpowers/` is gitignored; `git add -f` only when versioning those files.
- Commit style: `feat:` / `fix:` / `test:` / `docs:` with a short why.
- After every `cargo test` step, previously passing tests must still pass.

---

## File map

| Path | Responsibility |
|---|---|
| `src/app.rs` | `SEND_PENDING` / `SEND_FAIL` constants; `TelegramSink::react`; `FakeTg.reactions`; rewrite `send_and_ack`; update unit tests |
| `src/telegram/sink.rs` | `RealTg::react` via `set_message_reaction` |
| `src/actions.rs` | Update `send_by_number_creates_and_acks` assertions only |
| `src/telegram/tests.rs` | Update success-ack assertions to expect reactions |

---

### Task 1: `TelegramSink::react` + FakeTg + constants

**Files:**
- Modify: `src/app.rs` (constants, trait, `FakeTg`)

**Interfaces:**
- Consumes: existing `TelegramSink`, `AppError`
- Produces:
  - `pub const SEND_PENDING: &str = "📨";`
  - `pub const SEND_ACK: &str = "✅";` (already exists)
  - `pub const SEND_FAIL: &str = "❌";`
  - `TelegramSink::react(&self, message_id: i32, emoji: &str) -> Result<(), AppError>`
  - `FakeTg.reactions: Mutex<Vec<(i32, String)>>` — `(message_id, emoji)` in call order

- [ ] **Step 1: Write failing test** in `src/app.rs` `tests` module (near other FakeTg-using tests)

```rust
#[tokio::test]
async fn fake_tg_react_records_emoji() {
    let tg = FakeTg::new();
    tg.react(7, SEND_PENDING).await.unwrap();
    tg.react(7, SEND_ACK).await.unwrap();
    assert_eq!(
        tg.reactions.lock().unwrap().as_slice(),
        &[(7, SEND_PENDING.into()), (7, SEND_ACK.into())]
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib app::tests::fake_tg_react_records_emoji -- --nocapture`

Expected: FAIL (no `react` method / no `SEND_PENDING` / no `reactions` field)

- [ ] **Step 3: Minimal implementation**

Add constants next to `SEND_ACK`:

```rust
pub const SEND_PENDING: &str = "📨";
pub const SEND_ACK: &str = "✅";
pub const SEND_FAIL: &str = "❌";
```

Extend trait:

```rust
#[async_trait::async_trait]
pub trait TelegramSink: Send + Sync {
    async fn post(&self, thread_id: i32, text: String) -> Result<(), AppError>;
    async fn reply(&self, thread_id: i32, text: String, _reply_to: i32) -> Result<(), AppError> {
        self.post(thread_id, text).await
    }
    async fn react(&self, _message_id: i32, _emoji: &str) -> Result<(), AppError> {
        Err(AppError::Telegram("react not supported".into()))
    }
    async fn create_topic(&self, title: String) -> Result<i32, AppError>;
}
```

Update `FakeTg`:

```rust
pub struct FakeTg {
    pub posts: Mutex<Vec<(i32, String)>>,
    pub replies: Mutex<Vec<(i32, String, i32)>>,
    pub reactions: Mutex<Vec<(i32, String)>>,
    pub next_thread: AtomicI32,
    pub fail: bool,
}

impl FakeTg {
    pub fn new() -> Self {
        Self {
            posts: Mutex::new(Vec::new()),
            replies: Mutex::new(Vec::new()),
            reactions: Mutex::new(Vec::new()),
            next_thread: AtomicI32::new(100),
            fail: false,
        }
    }
}
```

Implement `react` on `FakeTg` (same `fail` gate as `post`/`reply`):

```rust
async fn react(&self, message_id: i32, emoji: &str) -> Result<(), AppError> {
    if self.fail {
        return Err(AppError::Telegram("fail".into()));
    }
    self.reactions
        .lock()
        .expect("fake tg reactions lock")
        .push((message_id, emoji.to_string()));
    Ok(())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib app::tests::fake_tg_react_records_emoji -- --nocapture`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "$(cat <<'EOF'
feat: add TelegramSink::react and send status emoji constants

EOF
)"
```

---

### Task 2: `send_and_ack` reaction lifecycle

**Files:**
- Modify: `src/app.rs` (`send_and_ack`, existing tests)

**Interfaces:**
- Consumes: `TelegramSink::react`, `SEND_PENDING` / `SEND_ACK` / `SEND_FAIL`, existing `ack_send`
- Produces: updated `send_and_ack` behavior per Global Constraints

- [ ] **Step 1: Rewrite failing/updated tests** in `src/app.rs`

Replace `send_and_ack_ok_records_replies_and_deletes` body expectations:

```rust
#[tokio::test]
async fn send_and_ack_ok_reacts_and_deletes() {
    let db = Db::open_in_memory().unwrap();
    let tg = FakeTg::new();
    let modem = FakeModem::default();
    send_and_ack(&db, "+98912", "hi", 42, Some(7), &modem, &tg, true)
        .await
        .unwrap();
    assert_eq!(
        modem.sent.lock().unwrap().as_slice(),
        &[("+98912".into(), "hi".into())]
    );
    assert_eq!(
        tg.reactions.lock().unwrap().as_slice(),
        &[(7, SEND_PENDING.into()), (7, SEND_ACK.into())]
    );
    assert!(tg.replies.lock().unwrap().is_empty());
    assert!(tg.posts.lock().unwrap().is_empty());
    assert_eq!(
        modem.deleted.lock().unwrap().as_slice(),
        &["/fake/sms/1".into()] as &[String]
    );
    assert_eq!(db.last_outbound_ok().unwrap().unwrap().0, "+98912");
}
```

Add success without `reply_to` (text fallback):

```rust
#[tokio::test]
async fn send_and_ack_ok_without_reply_to_posts_ack() {
    let db = Db::open_in_memory().unwrap();
    let tg = FakeTg::new();
    let modem = FakeModem::default();
    send_and_ack(&db, "+98912", "hi", 42, None, &modem, &tg, true)
        .await
        .unwrap();
    assert!(tg.reactions.lock().unwrap().is_empty());
    assert_eq!(
        tg.posts.lock().unwrap().as_slice(),
        &[(42, SEND_ACK.into())]
    );
}
```

Add failure with `reply_to`:

```rust
#[tokio::test]
async fn send_and_ack_err_reacts_fail_and_replies_error() {
    let db = Db::open_in_memory().unwrap();
    let tg = FakeTg::new();
    let modem = FakeModem {
        fail: true,
        ..FakeModem::default()
    };
    let err = send_and_ack(&db, "+98912", "hi", 42, Some(7), &modem, &tg, true)
        .await
        .unwrap_err();
    assert!(matches!(err, ActionError::ModemFailed(_)));
    assert_eq!(
        tg.reactions.lock().unwrap().as_slice(),
        &[(7, SEND_PENDING.into()), (7, SEND_FAIL.into())]
    );
    assert_eq!(
        tg.replies.lock().unwrap().as_slice(),
        &[(42, "modem error: error".into(), 7)]
    );
    assert!(modem.deleted.lock().unwrap().is_empty());
}
```

Keep `send_and_ack_err_posts_error_without_delete` (no `reply_to`) as-is except assert reactions empty:

```rust
assert!(tg.reactions.lock().unwrap().is_empty());
```

Add pending-react abort:

```rust
#[tokio::test]
async fn send_and_ack_pending_react_fail_skips_modem() {
    let db = Db::open_in_memory().unwrap();
    let tg = FakeTg {
        fail: true,
        ..FakeTg::new()
    };
    let modem = FakeModem::default();
    let err = send_and_ack(&db, "+98912", "hi", 42, Some(7), &modem, &tg, true)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        ActionError::TelegramFailed { sent: false, .. }
    ));
    assert!(modem.sent.lock().unwrap().is_empty());
}
```

Update `owner_text_in_topic_sends_and_acks`:

```rust
assert_eq!(
    tg.reactions.lock().unwrap().as_slice(),
    &[(7, SEND_PENDING.into()), (7, SEND_ACK.into())]
);
assert!(tg.replies.lock().unwrap().is_empty());
```

Update `send_failure_posts_error_keeps_going`:

```rust
assert_eq!(
    tg.reactions.lock().unwrap().as_slice(),
    &[(7, SEND_PENDING.into()), (7, SEND_FAIL.into())]
);
let replies = tg.replies.lock().unwrap();
assert!(replies[0].1.contains("error"));
assert_eq!(replies[0].2, 7);
```

- [ ] **Step 2: Run targeted tests — expect FAIL**

Run:

```bash
cargo test --lib \
  app::tests::send_and_ack_ok_reacts_and_deletes \
  app::tests::send_and_ack_ok_without_reply_to_posts_ack \
  app::tests::send_and_ack_err_reacts_fail_and_replies_error \
  app::tests::send_and_ack_pending_react_fail_skips_modem \
  -- --nocapture
```

Expected: FAIL (still old reply-based success path / missing behavior)

- [ ] **Step 3: Implement `send_and_ack`**

Replace `send_and_ack` with:

```rust
pub async fn send_and_ack(
    db: &Db,
    e164: &str,
    text: &str,
    thread_id: i32,
    reply_to: Option<i32>,
    modem: &dyn SmsModem,
    tg: &dyn TelegramSink,
    delete_enabled: bool,
) -> Result<(), ActionError> {
    if let Some(id) = reply_to {
        if let Err(err) = tg.react(id, SEND_PENDING).await {
            return Err(ActionError::TelegramFailed {
                sent: false,
                message: err.to_string(),
            });
        }
    }

    match modem.send(e164, text).await {
        Ok(path) => {
            db.record_outbound(e164, text, "ok", thread_id)?;
            let ack_result = match reply_to {
                Some(id) => tg.react(id, SEND_ACK).await,
                None => ack_send(tg, thread_id, SEND_ACK, None).await,
            };
            if let Err(err) = ack_result {
                return Err(ActionError::TelegramFailed {
                    sent: true,
                    message: err.to_string(),
                });
            }
            maybe_delete(delete_enabled, modem, &path).await;
            Ok(())
        }
        Err(err) => {
            let err_s = err.to_string();
            db.record_outbound(e164, text, &err_s, thread_id)?;
            if let Some(id) = reply_to {
                let _ = tg.react(id, SEND_FAIL).await;
            }
            let _ = ack_send(tg, thread_id, &err_s, reply_to).await;
            Err(ActionError::ModemFailed(err_s))
        }
    }
}
```

- [ ] **Step 4: Run app tests**

Run: `cargo test --lib app::tests -- --nocapture`

Expected: all PASS

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "$(cat <<'EOF'
feat: ack SMS sends with Telegram reactions when reply_to exists

EOF
)"
```

---

### Task 3: `RealTg::react`

**Files:**
- Modify: `src/telegram/sink.rs`

**Interfaces:**
- Consumes: `TelegramSink::react` signature from Task 1
- Produces: live `set_message_reaction` implementation

- [ ] **Step 1: Implement `react` on `RealTg`**

```rust
use teloxide::types::{ChatId, MessageId, ReactionType, ReplyParameters, ThreadId};

// inside impl TelegramSink for RealTg:
async fn react(&self, message_id: i32, emoji: &str) -> Result<(), AppError> {
    self.bot
        .set_message_reaction(self.chat_id, MessageId(message_id))
        .reaction(vec![ReactionType::Emoji {
            emoji: emoji.to_string(),
        }])
        .await
        .map_err(|e| AppError::Telegram(e.to_string()))?;
    Ok(())
}
```

Keep existing `post` / `reply` / `create_topic` unchanged.

- [ ] **Step 2: Compile-check**

Run: `cargo test --lib --no-run`

Expected: compiles with no errors

- [ ] **Step 3: Commit**

```bash
git add src/telegram/sink.rs
git commit -m "$(cat <<'EOF'
feat: set Telegram message reactions from RealTg

EOF
)"
```

---

### Task 4: Update remaining success-ack tests

**Files:**
- Modify: `src/actions.rs` (`send_by_number_creates_and_acks`)
- Modify: `src/telegram/tests.rs` (all assertions expecting success text `✅` reply)

**Interfaces:**
- Consumes: `FakeTg.reactions`, `SEND_PENDING`, `SEND_ACK`
- Produces: tests aligned with reaction ack

- [ ] **Step 1: Update `src/actions.rs` assertion**

In `send_by_number_creates_and_acks`, replace replies check with:

```rust
assert_eq!(
    tg.reactions.lock().unwrap().as_slice(),
    &[(7, crate::app::SEND_PENDING.into()), (7, crate::app::SEND_ACK.into())]
);
assert!(tg.replies.lock().unwrap().is_empty());
```

- [ ] **Step 2: Update `src/telegram/tests.rs`**

For each success path that asserted `replies` containing `✅` with a `reply_to`, assert reactions instead. Patterns:

```rust
assert_eq!(
    tg.reactions.lock().unwrap().as_slice(),
    &[(7, crate::app::SEND_PENDING.into()), (7, crate::app::SEND_ACK.into())]
);
assert!(tg.replies.lock().unwrap().is_empty());
```

Known sites (message ids vary — use the `reply_to` each test passes):

| Test | `reply_to` |
|---|---|
| `handle_sms` success (~line 183) | `7` |
| `num_callback_sends_pending_text` | `11` |
| `sms` create-topic success (~line 433) | `7` |
| `sms_does_not_apply_incoming_default` (~line 477) | `7` |

For `num_callback_sends_pending_text`:

```rust
assert_eq!(
    tg.reactions.lock().unwrap().as_slice(),
    &[(11, crate::app::SEND_PENDING.into()), (11, crate::app::SEND_ACK.into())]
);
assert!(tg.replies.lock().unwrap().is_empty());
```

- [ ] **Step 3: Run full test suite**

Run: `cargo test --lib -- --nocapture`

Expected: all PASS

- [ ] **Step 4: Commit**

```bash
git add src/actions.rs src/telegram/tests.rs
git commit -m "$(cat <<'EOF'
test: expect reaction acks instead of success text replies

EOF
)"
```

---

## Spec coverage (self-review)

| Spec requirement | Task |
|---|---|
| `📨` before send | Task 2 |
| `✅` on success, no success text when `reply_to` | Task 2 |
| `❌` + error text on failure when `reply_to` | Task 2 |
| No `reply_to` → text fallback | Task 2 |
| `TelegramSink::react` + Fake/Real | Tasks 1, 3 |
| `📨` fail → `TelegramFailed { sent: false }`, no modem | Task 2 |
| Final `✅` fail → `TelegramFailed { sent: true }` | Task 2 (same branch as today) |
| Tests for sequences / fallback / pending fail | Tasks 2, 4 |
| No DB/HTTP/OpenAPI | (none) |

No placeholders remaining. Signatures consistent: `react(message_id: i32, emoji: &str)`.
