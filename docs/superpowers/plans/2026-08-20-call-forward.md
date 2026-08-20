# Unconditional Call Forward Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Query, set, and disable SIM unconditional call forwarding (CFU) via ModemManager USSD, exposed on Telegram `/status` + interactive `/forward` and on `GET`/`PUT /api/v1/call-forward`.

**Architecture:** Pure USSD code builders + response parser; `CallForward` trait with Fake + ModemManager (`Modem.Modem3gpp.Ussd`) impls; shared `actions`; Telegram soft-fails forward on `/status`; HTTP keeps `/api/v1/status` unchanged and uses dedicated call-forward routes. Ephemeral `pending_forward` SQLite table prevents typed search/number text from becoming SMS.

**Tech Stack:** Rust 2021, Tokio, zbus (ModemManager), teloxide, axum 0.8, rusqlite, serde.

**Spec:** `docs/superpowers/specs/2026-08-20-call-forward-design.md`

## Global Constraints

- Unconditional CFU only (`*#21#` / `*21*<digits>#` / `#21#`). No conditional reasons.
- Network query is source of truth — do not store desired forward state as authority.
- HTTP `GET /api/v1/status` schema and behavior must not change.
- Auth: existing `X-Api-Key` middleware and error envelope.
- PUT: `e164` key required; `null` disables; empty string → 400; omitted key → 400.
- HTTP: modem offline → `503`; USSD/parse failure → `500` (do not reuse SMS `modem_failed` → 502 for these routes).
- Telegram `/status`: soft-fail forward to `unavailable`; never fail the whole status.
- USSD enable code uses digits only (strip leading `+` from E.164).
- `docs/superpowers/` is gitignored; `git add -f` only files you intend to version.
- Commit style: `feat:` / `fix:` / `docs:` with a short why.
- After every `cargo test` step, previously passing tests must still pass.

---

## File map

| Path | Responsibility |
|---|---|
| `src/call_forward.rs` | `CallForwardState`, USSD string builders, response parser |
| `src/modem.rs` | `CallForward` trait; `FakeModem` impl |
| `src/modem_mm.rs` | USSD D-Bus proxy; `CallForward` for `MmModem` |
| `src/status.rs` | Forward line on snapshot/HTML; soft-fail in `gather` |
| `src/actions.rs` | `get_call_forward` / `set_call_forward` / `disable_call_forward` |
| `src/db.rs` | `pending_forward` table + get/set/clear |
| `src/http.rs` | `GET`/`PUT /api/v1/call-forward`; `HttpState.forward` |
| `src/telegram/parse.rs` | `/forward` help + bot command; callback parsers |
| `src/telegram/keyboards.rs` | Forward keyboard |
| `src/telegram/handlers.rs` | `/forward`, callbacks, pending text intercept |
| `src/main.rs` | Wire `CallForward` into HTTP + Telegram dispatch |
| `src/lib.rs` | `pub mod call_forward` |
| `docs/openapi.yaml` | Call forward tag + paths |
| `README.md` | Command + API rows |

---

### Task 1: USSD builders and response parser

**Files:**
- Create: `src/call_forward.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `crate::normalize::normalize_e164`
- Produces:
  - `CallForwardState { enabled: bool, e164: Option<String> }`
  - `ussd_query() -> &'static str` (`*#21#`)
  - `ussd_disable() -> &'static str` (`#21#`)
  - `ussd_enable(e164: &str) -> String` (digits only after stripping `+`)
  - `parse_ussd_reply(text: &str, default_region: &str) -> Result<CallForwardState, String>`

- [ ] **Step 1: Write failing tests** in `src/call_forward.rs`

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ussd_codes() {
        assert_eq!(ussd_query(), "*#21#");
        assert_eq!(ussd_disable(), "#21#");
        assert_eq!(ussd_enable("+989121234567"), "*21*989121234567#");
    }

    #[test]
    fn parse_disabled_phrases() {
        for s in [
            "Call Forwarding Unconditional Not Forwarded",
            "CFU deactivated",
            "not forwarded",
            "disabled",
        ] {
            let st = parse_ussd_reply(s, "IR").unwrap();
            assert!(!st.enabled, "{s}");
            assert!(st.e164.is_none(), "{s}");
        }
    }

    #[test]
    fn parse_enabled_with_plus_e164() {
        let st = parse_ussd_reply(
            "Call Forwarding Unconditional +989121234567",
            "IR",
        )
        .unwrap();
        assert!(st.enabled);
        assert_eq!(st.e164.as_deref(), Some("+989121234567"));
    }

    #[test]
    fn parse_enabled_local_digits() {
        let st = parse_ussd_reply("Forwarded to 09121234567", "IR").unwrap();
        assert!(st.enabled);
        assert_eq!(st.e164.as_deref(), Some("+989121234567"));
    }

    #[test]
    fn parse_garbage_errors() {
        assert!(parse_ussd_reply("asdf qwerty", "IR").is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib call_forward::tests -- --nocapture`

Expected: compile error (`call_forward` module missing) or FAIL.

- [ ] **Step 3: Implement module**

```rust
// src/call_forward.rs
use serde::Serialize;

use crate::normalize::normalize_e164;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CallForwardState {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub e164: Option<String>,
}

pub fn ussd_query() -> &'static str {
    "*#21#"
}

pub fn ussd_disable() -> &'static str {
    "#21#"
}

pub fn ussd_enable(e164: &str) -> String {
    let digits: String = e164.chars().filter(|c| c.is_ascii_digit()).collect();
    format!("*21*{digits}#")
}

pub fn parse_ussd_reply(text: &str, default_region: &str) -> Result<CallForwardState, String> {
    let lower = text.to_ascii_lowercase();
    let disabled_markers = [
        "not forwarded",
        "deactivated",
        "disabled",
        "not active",
        "erased",
        "cancelled",
        "canceled",
    ];
    if disabled_markers.iter().any(|m| lower.contains(m)) {
        return Ok(CallForwardState {
            enabled: false,
            e164: None,
        });
    }

    // Prefer +E.164, else a long digit run (normalize).
    if let Some(plus) = extract_plus_number(text) {
        let e164 = normalize_e164(&plus, default_region).map_err(|e| e.to_string())?;
        return Ok(CallForwardState {
            enabled: true,
            e164: Some(e164),
        });
    }
    if let Some(digits) = extract_digit_run(text) {
        let e164 = normalize_e164(&digits, default_region).map_err(|e| e.to_string())?;
        return Ok(CallForwardState {
            enabled: true,
            e164: Some(e164),
        });
    }

    let enabled_markers = ["forwarded", "activated", "active", "unconditional"];
    if enabled_markers.iter().any(|m| lower.contains(m)) {
        return Err(format!("ussd reply looks enabled but has no number: {text}"));
    }
    Err(format!("unparseable ussd reply: {text}"))
}

fn extract_plus_number(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i - start > 8 {
                return Some(text[start..i].to_string());
            }
        } else {
            i += 1;
        }
    }
    None
}

fn extract_digit_run(text: &str) -> Option<String> {
    let mut best: Option<String> = None;
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() {
            cur.push(c);
        } else if !cur.is_empty() {
            if cur.len() >= 10 && best.as_ref().map(|b| b.len()).unwrap_or(0) < cur.len() {
                best = Some(cur.clone());
            }
            cur.clear();
        }
    }
    if cur.len() >= 10 && best.as_ref().map(|b| b.len()).unwrap_or(0) < cur.len() {
        best = Some(cur);
    }
    best
}
```

Add `pub mod call_forward;` to `src/lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib call_forward::tests -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -f src/call_forward.rs src/lib.rs
git commit -m "$(cat <<'EOF'
feat: add CFU USSD builders and reply parser

EOF
)"
```

---

### Task 2: `CallForward` trait and FakeModem

**Files:**
- Modify: `src/modem.rs`

**Interfaces:**
- Consumes: `CallForwardState`, `ModemError`
- Produces:
  ```rust
  #[async_trait::async_trait]
  pub trait CallForward: Send + Sync {
      async fn query_forward(&self, default_region: &str) -> Result<CallForwardState, ModemError>;
      async fn set_forward(&self, e164: &str, default_region: &str) -> Result<CallForwardState, ModemError>;
      async fn disable_forward(&self, default_region: &str) -> Result<CallForwardState, ModemError>;
  }
  ```
  - `FakeModem` fields: `forward: Mutex<CallForwardState>`, `forward_fail: bool`

- [ ] **Step 1: Write failing FakeModem tests** in `src/modem.rs` `mod tests`

```rust
#[tokio::test]
async fn fake_call_forward_set_query_disable() {
    let m = FakeModem::default();
    assert!(!m.query_forward("IR").await.unwrap().enabled);
    let set = m.set_forward("+989121234567", "IR").await.unwrap();
    assert!(set.enabled);
    assert_eq!(set.e164.as_deref(), Some("+989121234567"));
    assert_eq!(m.query_forward("IR").await.unwrap(), set);
    let off = m.disable_forward("IR").await.unwrap();
    assert!(!off.enabled);
    assert!(off.e164.is_none());
}

#[tokio::test]
async fn fake_call_forward_fail() {
    let m = FakeModem {
        forward_fail: true,
        ..FakeModem::default()
    };
    assert!(m.query_forward("IR").await.is_err());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib modem::tests::fake_call_forward -- --nocapture`

Expected: compile error (`CallForward` not found / method missing).

- [ ] **Step 3: Implement trait + FakeModem**

Add to `FakeModem`:

```rust
pub forward: Mutex<crate::call_forward::CallForwardState>,
pub forward_fail: bool,
```

In `Default`, initialize `forward` to `CallForwardState { enabled: false, e164: None }`, `forward_fail: false`.

```rust
#[async_trait::async_trait]
impl CallForward for FakeModem {
    async fn query_forward(&self, _default_region: &str) -> Result<CallForwardState, ModemError> {
        if self.forward_fail {
            return Err(ModemError::Failed("forward fail".into()));
        }
        Ok(self.forward.lock().expect("forward lock").clone())
    }

    async fn set_forward(&self, e164: &str, default_region: &str) -> Result<CallForwardState, ModemError> {
        if self.forward_fail {
            return Err(ModemError::Failed("forward fail".into()));
        }
        let e164 = crate::normalize::normalize_e164(e164, default_region)
            .map_err(|e| ModemError::Failed(e.to_string()))?;
        let st = CallForwardState {
            enabled: true,
            e164: Some(e164),
        };
        *self.forward.lock().expect("forward lock") = st.clone();
        Ok(st)
    }

    async fn disable_forward(&self, _default_region: &str) -> Result<CallForwardState, ModemError> {
        if self.forward_fail {
            return Err(ModemError::Failed("forward fail".into()));
        }
        let st = CallForwardState {
            enabled: false,
            e164: None,
        };
        *self.forward.lock().expect("forward lock") = st.clone();
        Ok(st)
    }
}
```

Import `CallForwardState` and `CallForward` as needed. Fix any `FakeModem { ..Default::default() }` struct literals in the codebase that break after new fields (prefer `..Default::default()`).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib modem::tests -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/modem.rs
git commit -m "$(cat <<'EOF'
feat: add CallForward trait and FakeModem support

EOF
)"
```

---

### Task 3: Actions + ActionError mapping for call forward

**Files:**
- Modify: `src/actions.rs`
- Modify: `src/http.rs` (`action_to_response` only — new variants)

**Interfaces:**
- Consumes: `dyn CallForward`, `normalize_e164`
- Produces:
  - `ActionError::ModemUnavailable(String)` → HTTP 503
  - `ActionError::ForwardFailed(String)` → HTTP 500
  - `pub async fn get_call_forward(forward: &dyn CallForward, region: &str) -> Result<CallForwardState, ActionError>`
  - `pub async fn put_call_forward(forward: &dyn CallForward, region: &str, e164: Option<String>) -> Result<CallForwardState, ActionError>`
    - `None` → disable; `Some` → set (normalize first)

- [ ] **Step 1: Write failing action tests** in `src/actions.rs` `mod tests`

```rust
#[tokio::test]
async fn put_call_forward_set_and_disable() {
    let m = crate::modem::FakeModem::default();
    let on = put_call_forward(&m, "IR", Some("09121234567".into()))
        .await
        .unwrap();
    assert_eq!(on.e164.as_deref(), Some("+989121234567"));
    let off = put_call_forward(&m, "IR", None).await.unwrap();
    assert!(!off.enabled);
}

#[tokio::test]
async fn get_call_forward_maps_modem_fail() {
    let m = crate::modem::FakeModem {
        forward_fail: true,
        ..Default::default()
    };
    let err = get_call_forward(&m, "IR").await.unwrap_err();
    assert!(matches!(err, ActionError::ForwardFailed(_)));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib actions::tests::put_call_forward -- --nocapture`

Expected: compile error.

- [ ] **Step 3: Implement actions + error mapping**

```rust
pub async fn get_call_forward(
    forward: &dyn crate::modem::CallForward,
    region: &str,
) -> Result<crate::call_forward::CallForwardState, ActionError> {
    forward
        .query_forward(region)
        .await
        .map_err(map_forward_err)
}

pub async fn put_call_forward(
    forward: &dyn crate::modem::CallForward,
    region: &str,
    e164: Option<String>,
) -> Result<crate::call_forward::CallForwardState, ActionError> {
    match e164 {
        None => forward
            .disable_forward(region)
            .await
            .map_err(map_forward_err),
        Some(raw) => {
            let e164 = normalize_e164(&raw, region)
                .map_err(|e| ActionError::InvalidNumber(e.to_string()))?;
            forward
                .set_forward(&e164, region)
                .await
                .map_err(map_forward_err)
        }
    }
}

fn map_forward_err(err: crate::modem::ModemError) -> ActionError {
    match err {
        crate::modem::ModemError::NotFound(msg) => ActionError::ModemUnavailable(msg),
        crate::modem::ModemError::Failed(msg) => ActionError::ForwardFailed(msg),
    }
}
```

Add enum variants. In `http.rs` `action_to_response`:

```rust
ActionError::ModemUnavailable(msg) => (
    StatusCode::SERVICE_UNAVAILABLE,
    Json(json!({ "error": "modem_unavailable", "message": msg })),
),
ActionError::ForwardFailed(msg) => (
    StatusCode::INTERNAL_SERVER_ERROR,
    Json(json!({ "error": "forward_failed", "message": msg })),
),
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib actions::tests::put_call_forward actions::tests::get_call_forward -- --nocapture`

Expected: PASS. Also `cargo test --lib` to ensure match exhaustiveness elsewhere.

- [ ] **Step 5: Commit**

```bash
git add src/actions.rs src/http.rs
git commit -m "$(cat <<'EOF'
feat: add call-forward actions and HTTP error mapping

EOF
)"
```

---

### Task 4: Status snapshot forward line (soft-fail)

**Files:**
- Modify: `src/status.rs`
- Modify: callers of `gather` / `actions::status` / `post_status` (signature updates only; wire real forward in Task 8 — temporarily pass `&FakeModem` or `&dyn CallForward` from updated call sites)

**Interfaces:**
- Consumes: `dyn CallForward`
- Produces:
  - `enum ForwardView { Off, On { label: String }, Unavailable }` on `StatusSnapshot`
  - HTML line after Modem block
  - `gather(..., forward: &dyn CallForward, region: &str, ...)` soft-catches query errors

- [ ] **Step 1: Write failing HTML tests** in `src/status.rs` `mod tests`

Update `happy()` / existing snapshots to include `forward: ForwardView::Off`. Add:

```rust
#[test]
fn forward_on_html() {
    let mut s = happy();
    s.forward = ForwardView::On {
        label: "Ali".into(),
    };
    let html = format_status_html(&s);
    assert!(html.contains("↪️ Forward · Ali"));
}

#[test]
fn forward_unavailable_html() {
    let mut s = happy();
    s.forward = ForwardView::Unavailable;
    assert!(format_status_html(&s).contains("↪️ Forward · unavailable"));
}

#[tokio::test]
async fn gather_soft_fails_forward() {
    let db = crate::db::Db::open_in_memory().unwrap();
    let modem = crate::modem::FakeModem::default();
    *modem.live.lock().unwrap() = Some(live_ok());
    let forward = crate::modem::FakeModem {
        forward_fail: true,
        ..Default::default()
    };
    let now = Utc.with_ymd_and_hms(2026, 8, 19, 11, 0, 0).unwrap();
    let snap = gather(
        &modem,
        &forward,
        "IR",
        &db,
        chrono_tz::UTC,
        "dwm222",
        now,
    )
    .await
    .unwrap();
    assert!(matches!(snap.forward, ForwardView::Unavailable));
}
```

Update `happy_path_html` expected string to include `\n↪️ Forward · off` after the SIM line (before the blank line / Today section).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib status::tests::forward_ -- --nocapture`

Expected: FAIL / compile error.

- [ ] **Step 3: Implement forward view in status**

```rust
pub enum ForwardView {
    Off,
    On { label: String },
    Unavailable,
}

// In format_modem_section, after sim_line / offline block, callers should append
// forward via format_status_html:

fn format_forward_line(view: &ForwardView) -> String {
    match view {
        ForwardView::Off => "↪️ Forward · off".into(),
        ForwardView::On { label } => format!("↪️ Forward · {}", html_escape(label)),
        ForwardView::Unavailable => "↪️ Forward · unavailable".into(),
    }
}
```

In `format_status_html`, after `format_modem_section`, push `\n` + forward line, then existing `\n\n` + Today.

In `gather`:

```rust
pub async fn gather(
    modem: &dyn ModemInfo,
    forward: &dyn crate::modem::CallForward,
    region: &str,
    db: &Db,
    tz: chrono_tz::Tz,
    modem_uid: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<StatusSnapshot, crate::db::DbError> {
    // ... existing modem + counts ...
    let forward_view = match forward.query_forward(region).await {
        Ok(st) if !st.enabled => ForwardView::Off,
        Ok(st) => {
            let e164 = st.e164.unwrap_or_default();
            let label = db
                .find_contact_by_e164(&e164)?
                .map(|c| c.display_name)
                .unwrap_or(e164);
            ForwardView::On { label }
        }
        Err(err) => {
            tracing::warn!(error = %err, "status call forward query");
            ForwardView::Unavailable
        }
    };
    Ok(StatusSnapshot { /* ..., */ forward: forward_view })
}
```

Update `actions::status` to take `forward: &dyn CallForward` and `region`, pass into `gather`.

**Keep the tree compiling in this commit:** add `forward: Arc<dyn CallForward>` to `HttpState`, Telegram `dispatch` / handler deps, and `main.rs` (`let forward: Arc<dyn CallForward> = mm.clone()`). Until Task 8’s real USSD impl, add a **temporary** `impl CallForward for MmModem` that returns `ModemError::Failed("call forward not implemented".into())` for all three methods (Telegram `/status` soft-fails to `unavailable`; HTTP forward routes return 500). Task 8 replaces that stub.

Update every `gather` / `status` / `post_status` / test call site in this same commit.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib status::tests -- --nocapture`

Expected: PASS. Also `cargo test --lib` must compile.

- [ ] **Step 5: Commit**

```bash
git add src/status.rs src/actions.rs src/http.rs src/telegram/handlers.rs src/main.rs src/modem_mm.rs
git commit -m "$(cat <<'EOF'
feat: show call forward line on status snapshot

EOF
)"
```

---

### Task 5: HTTP `GET`/`PUT /api/v1/call-forward`

**Files:**
- Modify: `src/http.rs`
- Modify: `docs/openapi.yaml`

**Interfaces:**
- Consumes: `actions::get_call_forward`, `put_call_forward`
- Produces: routes on `router`; `HttpState { forward: Arc<dyn CallForward>, ... }`

- [ ] **Step 1: Write failing axum tests** in `src/http.rs` `mod tests`

Extend `HttpState` construction in `test_router` with `forward: Arc::new(FakeModem::default())` (same instance as modem if desired: one `Arc<FakeModem>` cast to all traits).

```rust
#[tokio::test]
async fn call_forward_get_put() {
    let app = test_router("k");
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/call-forward")
                .header("X-Api-Key", "k")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["enabled"], false);

    let res = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/call-forward")
                .header("X-Api-Key", "k")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"e164":"09121234567"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let res = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/call-forward")
                .header("X-Api-Key", "k")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"e164":null}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn call_forward_put_omitted_e164_is_400() {
    let app = test_router("k");
    let res = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/call-forward")
                .header("X-Api-Key", "k")
                .header("content-type", "application/json")
                .body(Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}
```

Note: `oneshot` consumes the router — build `test_router` once per request or use `into_service` + clone pattern already used in this file. Follow existing test helpers in `http.rs`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib http::tests::call_forward -- --nocapture`

Expected: FAIL (404 / compile).

- [ ] **Step 3: Implement routes**

```rust
.route("/call-forward", get(call_forward_get).put(call_forward_put))
```

```rust
async fn call_forward_get(State(state): State<HttpState>) -> Response {
    match actions::get_call_forward(state.forward.as_ref(), &state.cfg.default_region).await {
        Ok(st) => Json(json!({
            "enabled": st.enabled,
            "e164": st.e164,
        }))
        .into_response(),
        Err(e) => action_to_response(e).into_response(),
    }
}

async fn call_forward_put(State(state): State<HttpState>, body: Result<Json<Value>, JsonRejection>) -> Response {
    let Json(v) = match body {
        Ok(j) => j,
        Err(rej) => return /* existing validation rejection helper */,
    };
    let Some(e164_val) = v.get("e164") else {
        return action_to_response(ActionError::Validation("e164 is required".into())).into_response();
    };
    let e164 = match e164_val {
        Value::Null => None,
        Value::String(s) if s.trim().is_empty() => {
            return action_to_response(ActionError::Validation("e164 must not be empty".into())).into_response();
        }
        Value::String(s) => Some(s.clone()),
        _ => {
            return action_to_response(ActionError::Validation("e164 must be string or null".into())).into_response();
        }
    };
    match actions::put_call_forward(state.forward.as_ref(), &state.cfg.default_region, e164).await {
        Ok(st) => Json(json!({ "enabled": st.enabled, "e164": st.e164 })).into_response(),
        Err(e) => action_to_response(e).into_response(),
    }
}
```

Assert in a quick test that `GET /api/v1/status` JSON still has **no** `call_forward` / `forward` field.

Update OpenAPI: tag `Call forward`; paths `GET`/`PUT /api/v1/call-forward`; schema:

```yaml
CallForwardState:
  type: object
  properties:
    enabled: { type: boolean }
    e164: { type: string, nullable: true }
  required: [enabled, e164]
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib http::tests -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -f src/http.rs docs/openapi.yaml
git commit -m "$(cat <<'EOF'
feat: add call-forward HTTP GET and PUT API

EOF
)"
```

---

### Task 6: `pending_forward` SQLite state

**Files:**
- Modify: `src/db.rs`

**Interfaces:**
- Produces:
  ```rust
  pub enum PendingForwardMode { Search, Number }

  pub struct PendingForward {
      pub mode: PendingForwardMode,
      pub edit_chat_id: i64,
      pub edit_message_id: i32,
  }

  Db::set_pending_forward(thread_id, mode, edit_chat_id, edit_message_id)
  Db::take_pending_forward(thread_id) -> Option<PendingForward>
  Db::clear_pending_forward(thread_id)
  Db::get_pending_forward(thread_id) -> Option<PendingForward>
  ```

- [ ] **Step 1: Write failing Db tests**

```rust
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
```

- [ ] **Step 2: Run to verify fail**

Run: `cargo test --lib db::tests::pending_forward_roundtrip -- --nocapture`

Expected: FAIL / compile error.

- [ ] **Step 3: Implement**

In `from_conn` after other ALTERs / `execute_batch`:

```rust
conn.execute_batch(
    "CREATE TABLE IF NOT EXISTS pending_forward (
        thread_id INTEGER PRIMARY KEY,
        mode TEXT NOT NULL,
        edit_chat_id INTEGER NOT NULL,
        edit_message_id INTEGER NOT NULL
     );",
)?;
```

Store mode as `"search"` / `"number"`. `take` = select then delete.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib db::tests::pending_forward_roundtrip -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/db.rs
git commit -m "$(cat <<'EOF'
feat: store pending call-forward Telegram input state

EOF
)"
```

---

### Task 7: Telegram `/forward` UI and pending intercept

**Files:**
- Modify: `src/telegram/parse.rs`
- Modify: `src/telegram/keyboards.rs`
- Modify: `src/telegram/handlers.rs`
- Modify: `src/telegram/tests.rs`
- Modify: `src/telegram/mod.rs` if re-exports needed

**Interfaces:**
- Callback data:
  - `cf:type` — type number
  - `cf:search` — pick contact (await search text)
  - `cf:off` — disable
  - `cf:cancel` — clear pending
  - `cf:c:{contact_id}` — contact chosen (then numbers if multi)
  - `cf:n:{e164}` — number chosen to set
- `forward_keyboard()` → InlineKeyboardMarkup
- Intercept in `on_message` **before** `handle_owner_text` / SMS topic send: if `get_pending_forward(thread_id)` is Some, consume text.

- [ ] **Step 1: Write failing parse/keyboard/handler tests** in `src/telegram/tests.rs`

```rust
#[test]
fn bot_commands_include_forward() {
    assert!(bot_commands().iter().any(|c| c.command == "forward"));
}

#[test]
fn parse_cf_callbacks() {
    assert_eq!(parse_cf_callback("cf:off"), Some(CfAction::Disable));
    assert_eq!(parse_cf_callback("cf:cancel"), Some(CfAction::Cancel));
    assert_eq!(parse_cf_callback("cf:type"), Some(CfAction::TypeNumber));
    assert_eq!(parse_cf_callback("cf:search"), Some(CfAction::Search));
    assert_eq!(parse_cf_callback("cf:c:42"), Some(CfAction::Contact(42)));
    assert_eq!(
        parse_cf_callback("cf:n:+989121234567"),
        Some(CfAction::Number("+989121234567".into()))
    );
}

#[test]
fn help_mentions_forward() {
    assert!(help_text().contains("/forward"));
}
```

Add an async handler test that: sets pending Number on thread 9, sends text `09121234567` through the intercept path with FakeModem, asserts forward enabled and pending cleared. Follow patterns in existing `telegram/tests.rs` (call handler helpers directly rather than full Dispatcher).

- [ ] **Step 2: Run to verify fail**

Run: `cargo test --lib telegram::tests::bot_commands_include_forward telegram::tests::parse_cf_callbacks -- --nocapture`

Expected: FAIL.

- [ ] **Step 3: Implement UI**

`parse.rs`: add command + help blurb; define `CfAction` + `parse_cf_callback`.

`keyboards.rs`:

```rust
pub(crate) fn forward_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([
        vec![
            InlineKeyboardButton::callback("Pick contact", "cf:search"),
            InlineKeyboardButton::callback("Type number", "cf:type"),
        ],
        vec![
            InlineKeyboardButton::callback("Disable", "cf:off"),
            InlineKeyboardButton::callback("Cancel", "cf:cancel"),
        ],
    ])
}
```

`handlers.rs`:

1. Add `forward: Arc<dyn CallForward>` to `on_message`, `on_callback`, `dispatch`, `schema` deps (teloxide dptree).
2. `Some("forward")` → query state, send HTML status + `forward_keyboard()`, store nothing yet.
3. Callbacks: set pending / disable / cancel / set from number / search hits via `search_keyboard` but callback prefix `cf:c:` (new keyboard helper `forward_search_keyboard` cloning search labels with `cf:c:{id}`).
4. Multi-number: `number_keyboard`-like with `cf:n:{e164}`.
5. At start of `on_message` after text extract:

```rust
if let Some(pending) = db.get_pending_forward(thread_id)? {
    // handle search vs number; edit pending.edit_* message; clear pending; return
}
```

6. `post_status` takes `forward` and passes to `gather`.
7. Format forward message body like status forward line + short instructions.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib telegram::tests -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/telegram/
git commit -m "$(cat <<'EOF'
feat: add interactive Telegram /forward command

EOF
)"
```

---

### Task 8: ModemManager USSD impl + process wiring

**Files:**
- Modify: `src/modem_mm.rs`
- Modify: `src/main.rs`
- Modify: any remaining call sites for `HttpState` / `dispatch`

**Interfaces:**
- Consumes: `call_forward::{ussd_*, parse_ussd_reply}`
- Produces: `impl CallForward for MmModem`

- [ ] **Step 1: Write a unit test for session helper if extracted**

Prefer testing pure “cancel then initiate then parse” via a small internal function is hard without D-Bus. Instead add a compile-only / documented manual check. Optional: test that `ussd_enable` is what `set_forward` would send by keeping MM impl thin.

Skip automated MM test; rely on Fake + manual hardware.

- [ ] **Step 2: Add USSD proxy**

```rust
#[proxy(
    interface = "org.freedesktop.ModemManager1.Modem.Modem3gpp.Ussd",
    default_service = "org.freedesktop.ModemManager1"
)]
trait Ussd {
    fn initiate(&self, command: &str) -> zbus::Result<String>;
    fn cancel(&self) -> zbus::Result<()>;
}
```

- [ ] **Step 3: Implement `CallForward` for `MmModem`**

```rust
const USSD_TIMEOUT: Duration = Duration::from_secs(45);

async fn ussd_roundtrip(&self, command: &str, region: &str) -> Result<CallForwardState, ModemError> {
    let path = self.ensure_path().await?; // use existing modem path resolution used by Messaging
    let conn = self.connection(); // existing field/accessor pattern in MmModem
    let ussd = UssdProxy::builder(&conn)
        .path(path.clone())
        .map_err(mm_err)?
        .build()
        .await
        .map_err(mm_err)?;
    let _ = ussd.cancel().await; // ignore stale-session errors
    let reply = tokio::time::timeout(USSD_TIMEOUT, ussd.initiate(command))
        .await
        .map_err(|_| ModemError::Failed("ussd timeout".into()))?
        .map_err(mm_err)?;
    let _ = ussd.cancel().await;
    parse_ussd_reply(&reply, region).map_err(ModemError::Failed)
}

#[async_trait::async_trait]
impl CallForward for MmModem {
    async fn query_forward(&self, default_region: &str) -> Result<CallForwardState, ModemError> {
        self.ussd_roundtrip(ussd_query(), default_region).await
    }

    async fn set_forward(&self, e164: &str, default_region: &str) -> Result<CallForwardState, ModemError> {
        let e164 = normalize_e164(e164, default_region)
            .map_err(|e| ModemError::Failed(e.to_string()))?;
        let apply = self
            .ussd_roundtrip(&ussd_enable(&e164), default_region)
            .await;
        // Prefer re-query; on query fail after ok apply, return requested state
        match self.query_forward(default_region).await {
            Ok(st) => Ok(st),
            Err(_) => match apply {
                Ok(st) => Ok(st),
                Err(e) => Err(e),
            },
        }
        // Better: if apply Err → return err; else query; if query Err → Ok(CallForwardState{enabled:true,e164:Some(e164)})
    }

    async fn disable_forward(&self, default_region: &str) -> Result<CallForwardState, ModemError> {
        let apply = self.ussd_roundtrip(ussd_disable(), default_region).await;
        match self.query_forward(default_region).await {
            Ok(st) => Ok(st),
            Err(_) => {
                apply?;
                Ok(CallForwardState {
                    enabled: false,
                    e164: None,
                })
            }
        }
    }
}
```

Rewrite `set_forward` clearly:

```rust
async fn set_forward(&self, e164: &str, default_region: &str) -> Result<CallForwardState, ModemError> {
    let e164 = normalize_e164(e164, default_region)
        .map_err(|e| ModemError::Failed(e.to_string()))?;
    self.ussd_roundtrip(&ussd_enable(&e164), default_region).await?;
    match self.query_forward(default_region).await {
        Ok(st) => Ok(st),
        Err(_) => Ok(CallForwardState {
            enabled: true,
            e164: Some(e164),
        }),
    }
}
```

Use the same path/connection helpers already used for `MessagingProxy` in this file (read `MmModem` struct — do not invent new connection management).

Wire `main.rs`:

```rust
let forward: Arc<dyn CallForward> = mm.clone();
// HttpState { ..., forward: forward.clone() }
// dispatch(cfg, db, modem, info, forward)
```

- [ ] **Step 4: Run full test suite**

Run: `cargo test --lib -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/modem_mm.rs src/main.rs src/http.rs src/telegram/handlers.rs src/actions.rs src/status.rs
git commit -m "$(cat <<'EOF'
feat: drive CFU over ModemManager USSD and wire daemon

EOF
)"
```

---

### Task 9: README docs

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update command table**

Add `/forward` row: “Interactive: set or disable unconditional call forwarding”.

Update `/status` row to mention call forward line.

- [ ] **Step 2: Update HTTP API table**

| `/api/v1/call-forward` | GET | Current unconditional forward |
| `/api/v1/call-forward` | PUT | Set `e164` or `null` to disable |

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "$(cat <<'EOF'
docs: document call-forward command and API

EOF
)"
```

---

### Task 10: Manual hardware verification (no commit required)

On the host with the DWM-222:

```bash
# Optional baseline via mmcli (same codes the bot will send)
mmcli -m <id> --3gpp-ussd-initiate='*#21#'
mmcli -m <id> --3gpp-ussd-cancel
```

Then via bot/API:

1. `GET /api/v1/call-forward` matches phone reality  
2. `PUT` set to a number you control; confirm ring-forward  
3. `PUT` `{ "e164": null }`; confirm forward off  
4. Telegram `/status` shows on/off  
5. `/forward` pick contact / type number / disable without sending SMS  

If Irancell reply text does not parse, capture the raw string, add a fixture to `call_forward::tests`, extend `parse_ussd_reply`, commit `fix: parse Irancell CFU USSD reply`.

---

## Spec coverage checklist

| Spec requirement | Task |
|---|---|
| USSD `*#21#` / `*21*` / `#21#` | 1, 8 |
| `CallForward` shared by TG + HTTP | 2, 3, 8 |
| Telegram `/status` forward line + soft-fail | 4, 7 |
| Interactive `/forward` (contact / type / disable / cancel) | 6, 7 |
| Pending input not sent as SMS | 6, 7 |
| `GET`/`PUT /api/v1/call-forward` | 5 |
| HTTP `/api/v1/status` unchanged | 5 (assert) |
| PUT null vs omit vs empty | 5 |
| 503 / 500 mapping | 3, 5 |
| Re-query after set/disable; fallback to requested | 8 |
| OpenAPI + README | 5, 9 |
| FakeModem tests | 2–5, 7 |
| Hardware validation | 10 |

## Plan self-review notes

- No TBD placeholders left for implementers; MM path/connection must follow existing `MmModem` helpers (read the struct before coding Task 8).
- `CallForwardState` / trait method names are consistent across tasks.
- Task 4 wires `Arc<dyn CallForward>` with a temporary `MmModem` stub so `main` stays green; Task 8 replaces the stub with USSD.
