# Chat History HTTP API

Date: 2026-08-20

Read-only JSON endpoints that expose local SMS history as a chat inbox and per-thread message timelines. Extends the existing HTTP API (`docs/superpowers/specs/2026-08-20-http-api-design.md`). Telegram remains the live UI; this API serves what the bot already recorded in SQLite.

## Goals

- List recent chats (forum topics with SMS activity), ordered by last SMS.
- List messages for one thread, including General (`thread_id = 1`).
- Reuse API key auth and the existing error body shape.
- Attribute SMS rows to a stable `thread_id` at write time.

## Non-goals

- Scraping Telegram Bot API / forum topic history
- Unread counts (response field reserved as `null`)
- Webhooks or push for new SMS
- Filtering the chats list by identity query params
- TLS / OAuth / per-client keys
- Changing inbound SMS → Telegram behavior beyond storing `thread_id`

## Approach

**Record `thread_id` on SMS write** (Approach B).

- Extend `inbound_log` and `outbound_log` with nullable `thread_id`.
- On every new inbound/outbound record, store the resolved forum thread (`1` = General).
- Best-effort startup backfill for older `NULL` rows via current E.164 → topic mapping.
- Query time uses `COALESCE(thread_id, 1)` so remaining unmapped rows land in General.

Rejected alternatives:

- Query-time join only: smaller change, but attribution drifts when numbers later link to contacts.
- New unified `message_log`: cleaner long-term model, more migration than v1 needs.

## Architecture

Same split as the HTTP API: read-only actions, no Telegram side effects.

```
GET /api/v1/chats ──┐
                    ├── actions::list_chats / list_messages
GET /api/v1/chats/{thread_id}/messages ──┘
                    │
                    └── Db (thread_id on inbound_log / outbound_log)
```

- **`actions`**: parse/validate pagination, call Db helpers, map rows → DTOs.
- **`http`**: `X-Api-Key`, path/query parse, status mapping.
- **Existing write paths**: pass resolved `thread_id` into `record_inbound` / outbound log insert.

## Data model

### Schema

```sql
ALTER TABLE inbound_log ADD COLUMN thread_id INTEGER;
ALTER TABLE outbound_log ADD COLUMN thread_id INTEGER;
```

Safe nullable columns. Pre-migration rows stay `NULL` until backfill.

### Write path

- **Inbound:** after `route_inbound` (or equivalent) chooses a thread, store that `thread_id` on insert. General destinations use `1`.
- **Outbound:** send path already knows the contact-topic `thread_id`; store it on outbound log insert.

### Backfill (startup, once per process is fine)

Fill `NULL` `thread_id` where an E.164 matches a topic (prefer topic default / contact numbers mapping already in Db). Rows still `NULL` after backfill are treated as General at query time via `COALESCE(thread_id, 1)`.

### Db helpers

- `chats_with_activity(limit, before, after) → Vec<ChatSummary>`
  - One row per `thread_id` that has any SMS.
  - Includes General (`1`) when it has SMS.
  - Ordered by last message time descending.
  - Carries last preview, direction, and topic metadata when available.
- `messages_for_thread(thread_id, limit, before, after) → Vec<MessageRow>`
  - Union of inbound + outbound for that thread (all numbers ever recorded under that `thread_id`).
  - Ordered by `created_at DESC`, then source id DESC for stable ties.
  - Cursor is timestamp-only (`before` / `after` exclusive). Same-second ties may reappear at a page boundary; acceptable for personal SMS volume.

## Routes

Prefix: `/api/v1`. Auth: `X-Api-Key` (same as existing API). Methods: `GET`.

Identity model for this feature:

- Chats list is global (no identity filter in v1).
- Messages are keyed by path `thread_id`. Optional query `number` / `contact_id` may be supplied for consistency checks; mismatch → `409 identity_conflict`. Clients that only know contact/number resolve via existing `/who` or `/open` first.

### `GET /api/v1/chats`

Topics with at least one SMS record, newest activity first. Unknown/ignored numbers without a contact topic are **bucketed into one General chat** (`thread_id = 1`), not listed as synthetic per-number chats.

| Query | Type | Default | Notes |
|---|---|---|---|
| `limit` | int | 50 | Hard max 100 |
| `before` | ISO-8601 string | — | Chats with `last_message_at` strictly before |
| `after` | ISO-8601 string | — | Chats with `last_message_at` strictly after |

```json
{
  "chats": [
    {
      "thread_id": 456,
      "title": "Ali (4567)",
      "contact_id": 42,
      "display_name": "Ali",
      "default_e164": "+98912…",
      "last_message_at": "2026-08-20T08:00:00Z",
      "last_message_preview": "On my way",
      "last_message_direction": "out",
      "unread_count": null
    },
    {
      "thread_id": 1,
      "title": "General",
      "contact_id": null,
      "display_name": null,
      "default_e164": null,
      "last_message_at": "2026-08-19T14:22:00Z",
      "last_message_preview": "Unknown caller",
      "last_message_direction": "in",
      "unread_count": null
    }
  ],
  "next_before": "2026-08-19T14:22:00Z"
}
```

`next_before` is the last item’s `last_message_at`, or `null` when there is no next page. `unread_count` is always `null` in v1.

### `GET /api/v1/chats/{thread_id}/messages`

`thread_id = 1` is always General (no `topics` row required). For any other id: if there is no `topics` row → `404 not_found`, even if somehow SMS rows exist. A known topic with zero SMS → `200` with `"messages": []`.

Includes **all** recorded SMS: inbound, outbound success, and outbound failure.

| Query | Type | Default | Notes |
|---|---|---|---|
| `limit` | int | 50 | Hard max 100 |
| `before` | ISO-8601 string | — | Messages with `timestamp` strictly before |
| `after` | ISO-8601 string | — | Messages with `timestamp` strictly after |
| `number` | string | — | Optional consistency check |
| `contact_id` | int | — | Optional consistency check |

`before` and `after` may both be set (closed range). If both parse and `before <= after` → `400 validation`. Same rule on `/chats`.

```json
{
  "thread_id": 456,
  "title": "Ali (4567)",
  "contact_id": 42,
  "messages": [
    {
      "id": "in:88",
      "direction": "in",
      "e164": "+98912…",
      "body": "On my way",
      "timestamp": "2026-08-20T08:00:00Z",
      "sms_ts": "2026-08-20T08:00:00Z",
      "status": "ok"
    },
    {
      "id": "out:55",
      "direction": "out",
      "e164": "+98912…",
      "body": "Got it",
      "timestamp": "2026-08-20T07:55:00Z",
      "sms_ts": null,
      "status": "ok"
    }
  ],
  "next_before": "2026-08-20T07:55:00Z"
}
```

Message field rules:

- `id`: `"in:<rowid>"` or `"out:<rowid>"` (two source tables).
- `direction`: `"in"` | `"out"`.
- `timestamp`: log `created_at` (ISO-8601).
- `sms_ts`: modem-reported original time when present (inbound); otherwise `null`.
- `status`: `"ok"` | `"failed"` (outbound `result ≠ "ok"` → `"failed"`; inbound always `"ok"`).
- `next_before`: last message `timestamp` in the page, or `null` if no more pages.

Empty history is success: `200` with `"chats": []` or `"messages": []`.

## Errors

Same envelope as the rest of the API:

```json
{
  "error": "not_found",
  "message": "unknown thread"
}
```

| HTTP | `error` | When |
|---|---|---|
| 401 | `unauthorized` | Bad or missing `X-Api-Key` |
| 400 | `validation` | Bad `limit`, unparseable `before`/`after`, or `before <= after` when both set |
| 404 | `not_found` | Path `thread_id` ≠ `1` and no matching `topics` row |
| 409 | `identity_conflict` | Optional `number` / `contact_id` disagree with path `thread_id` |
| 500 | `internal` | DB failure |

## Testing

- Db: migration/backfill; `COALESCE` into General; chat ordering; message union with in/out/failed; cursor boundaries for `before`/`after`.
- Actions/HTTP: auth required; 404 unknown thread; 200 empty lists; pagination page boundaries; optional identity conflict.
- No modem/Telegram mocks required for these read paths.

## Docs

- Extend `docs/openapi.yaml` with both routes and schemas.
- README HTTP API table: add the two GET routes.
- No new env vars.

## Success

A client with the API key can:

1. `GET /api/v1/chats` and see recent contact topics plus a General bucket when unknown SMS exists.
2. `GET /api/v1/chats/{thread_id}/messages` and page through that thread’s SMS timeline (including failed outbound).
3. Rely on new SMS continuing to carry the correct `thread_id` after deploy.
