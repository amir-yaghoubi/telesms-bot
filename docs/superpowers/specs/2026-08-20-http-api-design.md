# HTTP API for telesms-bot

Date: 2026-08-20

Personal JSON API that mirrors existing Telegram bot actions (except `/help`). Local and LAN clients (scripts, Home Assistant, n8n) call it with a shared API key. Telegram remains the conversation log.

## Goals

- Expose every owner action except `/help` as HTTP + JSON.
- Reuse one domain path for SMS routing, contacts, ignore, and status so Telegram and HTTP cannot diverge.
- Keep current Telegram-only deploys working when the key is unset.

## Non-goals

- `/help`
- Inbound SMS over HTTP (no webhook, no inbox poll)
- TLS / OAuth / per-client keys
- OpenAPI generation
- A second process or separate database
- Changing inbound SMS → Telegram behavior

## Architecture

One daemon. Split **policy** from **Telegram presentation**.

```
HTTP (axum) ──┐
              ├── actions (typed in → typed out)
Telegram ─────┘         │
                        ├── Db / route / normalize
                        ├── SmsModem
                        └── TelegramSink (topics, posts, ✅)
```

- **`actions`**: one function per API action. Side effects that exist today still happen here (modem send, SQLite, create/open forum topics, post SMS text and ✅ in the right thread).
- **Telegram adapter**: parse commands and callbacks, map the current forum thread to identity, call `actions`, then render buttons/HTML as today.
- **HTTP adapter**: auth, JSON, map HTTP errors. No inline keyboards.

HTTP runs as a Tokio task beside the existing JoinSet loops. `telegram::dispatch` stays the Ctrl-C owner.

If `API_KEY` is unset or empty, do not bind a port (Telegram-only, current behavior).

## Identity

Contact-scoped calls put identity fields at the **top level** of the JSON body. At least one of `number` or `contact_id` is required. `thread_id` is optional and only used to disambiguate or to choose which forum topic to post in; it is never enough on its own.

```json
{
  "number": "0912xxxxxxx",
  "contact_id": 42,
  "thread_id": 123
}
```

Resolution:

1. Normalize `number` with `DEFAULT_REGION` when present. Invalid number → `400 invalid_number`.
2. Load contact/topic from each provided field.
3. If two or more fields are set and they do not refer to the same contact/topic → `409 identity_conflict`.
4. `contact_id` unknown → `404 not_found`.
5. `thread_id` unknown or General (`1`) when a real topic is required → `404 not_found`.
6. `number` with no contact and no topic is allowed for **send** and **ignore** (unknown / ignored numbers). For **who**, **number list/set**, and **open**, a number that matches neither a contact nor a topic → `404 not_found`, except **open** which may create a topic (below).

Do not use Telegram `pending_outbound` for HTTP. If send-to-default has no default number, return `409 need_default_number` with the candidate list. The client sets a default via `POST /api/v1/number` and retries.

## Auth, bind, config

| Env | Default | Role |
|---|---|---|
| `API_KEY` | unset | Shared secret. Empty/unset → HTTP off |
| `API_BIND` | `0.0.0.0` | Listen address |
| `API_PORT` | `8787` | Listen port |

- Header: `X-Api-Key: <key>`
- Missing/wrong key → `401` with the standard error body. Compare in constant time; if lengths differ, still run a dummy compare.
- Unauthenticated `GET /health` returns `200` `{"ok":true}` only when the HTTP server is running (key is set). Useful for Compose; does not imply the modem is up.
- No TLS. LAN/VPN plus the key is the trust model.
- Compose: publish `8787:8787` and document `API_KEY` in `.env.example`. Native `cargo run` uses the same env.

## Routes

Prefix: `/api/v1`. JSON bodies: `Content-Type: application/json`.

`GET /health` is not under `/api/v1` and does not require a key.

### `GET /api/v1/status`

No body. Same data as `/status`, as JSON (not HTML).

Modem offline is success with `"modem": {"state": "offline"}`, not `5xx`.

Example live body (field set is normative; extra fields must not be required by clients):

```json
{
  "modem_uid": "dwm222",
  "modem": {
    "state": "connected",
    "operator": "MCI",
    "registration": "home",
    "signal_percent": 72,
    "rssi_dbm": -81,
    "access_tech": "lte",
    "sim": "ok"
  },
  "today_in": 3,
  "today_out_ok": 2,
  "today_out_fail": 0,
  "last_fail_error": null,
  "last_in": { "label": "Ali", "when": "5m ago" },
  "last_out": { "label": "+98912…", "when": "just now" },
  "contacts_ok": true
}
```

`access_tech` values: `gsm` | `umts` | `lte` | `nr`. `sim`: `ok` | `missing` | `pin_required`. `registration`: `home` | `roaming` | omitted. Modem `state` uses the existing `ModemState::label()` strings lowercased (`connected`, `searching`, …) plus `offline`.

### `POST /api/v1/sms`

Send SMS. Telegram still creates/opens the contact topic when routing says so, posts in that thread, and acks ✅ (or the modem error text).

```json
{
  "number": "0912xxxxxxx",
  "contact_id": null,
  "thread_id": null,
  "text": "hello"
}
```

Rules:

- `text` required, non-empty after trim; else `400 validation`.
- If `number` is set: send to that E.164 (same as `/sms <number> <text>`), including topic create/open.
- If `number` is omitted: `contact_id` is required (optional `thread_id`). Resolve to a topic and send to that topic’s default (same as typing in a contact topic). No default → `409 need_default_number` with `"numbers": ["+98…"]`.
- HTTP `200` when the modem accepted the send **and** Telegram ack succeeded. HTTP `502 modem_failed` when send fails (Telegram still gets the error text; outbound_log still records failure). If the modem accepted the send but Telegram ack fails, return `502 telegram_failed` with `"sent": true` in the error object (SMS may already be on the wire). Same ordering as today’s `send_and_ack`.

Success:

```json
{
  "ok": true,
  "e164": "+98912…",
  "thread_id": 456,
  "sent": true
}
```

### `POST /api/v1/search`

```json
{ "query": "ali" }
```

Empty query → `400 validation`. Contacts flag down → `503 contacts_unavailable`. No matches → `200` with `"contacts": []`.

Cap results at 20 (same as the Telegram keyboard).

```json
{
  "contacts": [
    {
      "id": 42,
      "display_name": "Ali",
      "numbers": ["+98912…"],
      "ambiguous": false
    }
  ]
}
```

### `POST /api/v1/open`

Create or open the forum topic, then return thread metadata. Same Telegram create/link behavior as `/open` / search tap.

Identity: `contact_id` and/or `number`. `thread_id` is ignored (you already have a topic).

- Existing topic for that contact/number: do not create a second topic; return it.
- Known Google contact without a topic: create topic (single number → default set; multiple → default unset), same as today.
- Number with no contact: create a number-titled topic with that default (same as `/sms` unknown-number path, without sending).

```json
{
  "contact_id": 42,
  "thread_id": 456,
  "title": "Ali +98912…",
  "created": false
}
```

### `POST /api/v1/who`

Identity required. General / unknown topic → `404`.

```json
{
  "thread_id": 456,
  "contact_id": 42,
  "display_name": "Ali",
  "numbers": ["+98912…"],
  "default_e164": "+98912…",
  "ambiguous": false
}
```

### `POST /api/v1/number`

List or set default.

**List:** identity only.

```json
{
  "thread_id": 456,
  "numbers": ["+98912…", "+98913…"],
  "default_e164": "+98912…"
}
```

Empty numbers → `200` with `"numbers": []` (Telegram posts “no numbers”; API does not need that string).

**Set:** identity plus `"default"` (raw or E.164). Number must be one of the topic’s numbers after normalize; else `400 validation`. Then `db.set_default_number`. Do **not** flush Telegram `pending_outbound` from HTTP (that queue is chat UX). Return the list shape above with the new default.

Telegram still posts `default is +E164` in the topic so the log stays consistent.

### `POST /api/v1/ignore`

Identity required.

- If `number` is set: ignore that E.164 only (General `/ignore` reply).
- Else: ignore every number on the resolved topic/contact (contact-topic `/ignore`).

Telegram still posts `ignored …` in the resolved thread when one exists; if there is no topic (ignore-by-number only), skip the Telegram post.

```json
{
  "ignored": ["+98912…"]
}
```

## Errors

All 4xx/5xx bodies:

```json
{
  "error": "not_found",
  "message": "unknown contact"
}
```

| HTTP | `error` | When |
|---|---|---|
| 401 | `unauthorized` | Bad or missing `X-Api-Key` |
| 400 | `validation` | Missing fields, empty text/query |
| 400 | `missing_identity` | No number/contact_id/thread_id where required |
| 400 | `invalid_number` | `normalize_e164` failed |
| 404 | `not_found` | Unknown contact/topic |
| 409 | `identity_conflict` | Fields disagree |
| 409 | `need_default_number` | Send-to-default with no default; include `"numbers"` |
| 503 | `contacts_unavailable` | Search while Google sync flag is down |
| 502 | `modem_failed` | `SmsModem::send` error |
| 502 | `telegram_failed` | Sink error (see `/sms` note) |

Unknown routes: `404` with `error: "not_found"`. Wrong method: `405`.

## Testing

- Identity resolver unit tests: number-only, contact-only, matching pair, conflicting pair, bad number, General thread.
- Each action: in-memory `Db` + `FakeModem` + `FakeTg`. Assert JSON-equivalent structs **and** Telegram posts/topics (log still updates).
- Axum tests: `401` without key on `/api/v1/*`; `200` with key on `/status`; `GET /health` returns `200` without `X-Api-Key`.
- Existing Telegram handler tests keep passing; they call `actions` or stay as adapter tests.
- Config: `API_KEY` unset skips bind (unit test with a fake listen flag, not a real port if awkward).

## Docs / deploy

- `.env.example`: `API_KEY`, `API_BIND`, `API_PORT`.
- README: short “HTTP API” table pointing at this spec’s routes.
- `compose.yaml`: `"8787:8787"`.

## Implementation sketch (not a plan)

Crate: `axum`, `tower-http` (trace). Optional `subtle` or `constant_time_eq` for the key.

New modules: `src/actions.rs` (or `src/actions/`), `src/http.rs`. Telegram handlers become thin. `Config` grows the three API fields. `main` spawns the HTTP server when the key is set, then `telegram::dispatch` as now.

## Success

A host script can search a contact, open their topic, set a default number, send SMS, ignore a number, and read status — all with JSON — while the forum group still shows the same topics and acks as if the owner had used the bot.
