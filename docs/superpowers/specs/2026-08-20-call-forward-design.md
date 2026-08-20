# Unconditional Call Forward Control

Date: 2026-08-20

Control SIM unconditional call forwarding (CFU / “forward all calls”) from Telegram and the HTTP API. Query and change the **network** state via ModemManager USSD — the same codes used on a phone today. HTTP `/api/v1/status` stays SMS/modem-only; Telegram `/status` gains a Call forward line; mutate/read of forwarding uses dedicated surfaces.

## Goals

- Show whether unconditional forward is on, and to which number, on Telegram `/status`.
- Interactive Telegram `/forward` to set (contact or typed number) or disable.
- `GET` / `PUT /api/v1/call-forward` for machine clients.
- Share one modem capability between Telegram and HTTP.
- Network query is the source of truth (no desired-state mirror as authority).

## Non-goals

- Conditional forwarding (busy / no-answer / unreachable)
- Changing HTTP `GET /api/v1/status` schema
- Storing desired forward state in SQLite as the truth
- Voice calls, call logs, or answering calls on the stick
- Carrier-specific portals or SMS-based forward management
- Multi-SIM / multi-modem

## Approach

**USSD via ModemManager** (Approach 1).

| Action  | USSD        |
|---------|-------------|
| Query   | `*#21#`     |
| Enable  | `*21*<e164>#` |
| Disable | `#21#`      |

Rejected alternatives:

- **AT+CCFC via `Command`:** more structured, but farther from current phone workflow and stick support varies; keep as a later fallback if USSD proves flaky on the DWM-222.
- **Desired-state in SQLite + apply/retry:** faster UI, but can disagree with the network; overkill for a personal gateway.

## Architecture

```
Telegram /status ──┐
Telegram /forward ─┼── actions::call_forward_* ── CallForward trait ── MM USSD
GET/PUT /api/v1/call-forward ──┘
```

- **`CallForward` trait** (`query` / `set` / `disable`) next to existing `ModemInfo` / `SmsModem`.
- **ModemManager impl** uses `Modem.Modem3gpp.Ussd` (initiate / respond / cancel as needed).
- **`FakeModem`** stub for tests.
- **`actions`**: normalize numbers, invoke trait, map errors; no USSD strings in handlers.
- **Telegram**: status line + interactive keyboard; short-lived pending-input flag so typed numbers are not sent as SMS.
- **HTTP**: dedicated routes only; same `X-Api-Key` and error envelope as the rest of `/api/v1/*`.

## Data model

No durable “forward target” table. Network state is authoritative.

**Pending Telegram input only** (ephemeral UX):

- Per-thread pending mode: idle | awaiting search | awaiting number.
- While awaiting search or number, that text is consumed for the forward flow (not outbound SMS).
- Cleared on successful set, Cancel, Disable success, or starting a new `/forward` flow.

Exact column/table shape is an implementation detail; keep it minimal (e.g. enum/text on `topics` or a tiny keyed pending row).

## Telegram

### `/status`

After the Modem block, one Call forward line:

- Active: `↪️ Forward · <label>` where `label` is contact display name if the E.164 is known, else the E.164
- Off: `↪️ Forward · off`
- Query failed: `↪️ Forward · unavailable` (do **not** fail the whole status snapshot)

Refresh (`st:r`) re-queries forward as well.

### `/forward`

Allowed wherever other commands are (forum group + authorized user).

Opens a message with current state and inline keyboard:

1. **Pick contact** — set pending to “awaiting search”; next text is a contact query (not SMS); show hit buttons like `/search`; multi-number contacts then get number buttons like `/number`.
2. **Type number** — set pending to “awaiting number”; next text is the candidate (not SMS); normalize; then set.
3. **Disable** — run `#21#`; edit message to show off.
4. **Cancel** — clear pending input; edit message to idle/cancelled.

After set/disable success, edit the `/forward` message to the new state and clear pending input.

Register bot command: `forward` — “Set or disable call forwarding”.

## HTTP API

Auth: `X-Api-Key` on `/api/v1/*`. **`GET /api/v1/status` unchanged.**

### `GET /api/v1/call-forward`

```json
{ "enabled": true, "e164": "+989121234567" }
```

or

```json
{ "enabled": false, "e164": null }
```

### `PUT /api/v1/call-forward`

- Set: `{ "e164": "+989121234567" }` (or local form; normalize with `DEFAULT_REGION`). Empty string is `400`.
- Disable: `{ "e164": null }` only (omit field → `400`).

Response: same shape as GET after apply. After set/disable, re-query via `*#21#`. If re-query fails but the apply reported success, return the requested state (`enabled`/`e164` as submitted).

When `enabled` is `true`, `e164` is always a non-null string.

### Errors

| Status | When |
|--------|------|
| 400 | Invalid `e164`, empty string, or field omitted on PUT |
| 401 | Bad or missing API key |
| 503 | Modem offline / not registered |
| 500 | USSD timeout, busy session, or unparseable network response |

Use the existing JSON error envelope. Update `docs/openapi.yaml` with a **Call forward** tag and schemas.

## USSD session and parsing

1. Require modem present and usable (same bar as other modem ops).
2. Cancel any stale USSD session if the interface allows.
3. Initiate the code; wait with a bounded timeout.
4. Parse network text into `{ enabled, e164? }`.
5. Always cancel/cleanup the session when finished.

**Parsing rules**

- Extract an E.164 or local digit sequence when present; normalize with `DEFAULT_REGION` when local.
- Map known “not forwarded / deactivated / disabled” style phrases → `enabled: false`, `e164: null`.
- Unparseable response → error (do not invent state).
- After enable/disable, if success text is ambiguous, follow with `*#21#` to confirm.

Carrier strings vary (e.g. Irancell). Ship unit tests with captured samples; extend the parser when real responses are collected on the live SIM.

## Error handling summary

| Surface | Modem / USSD failure |
|---------|----------------------|
| Telegram `/status` | Soft: `unavailable` line |
| Telegram `/forward` | Hard: show error on the message; clear pending only on success or Cancel |
| HTTP GET/PUT | Hard: 503 modem down; 500 USSD/parse failure |

## Testing

- Unit: USSD code builders, response parsers, status HTML forward line, JSON DTOs.
- Integration-style with `FakeModem`: Telegram callback paths and HTTP GET/PUT.
- Manual on hardware: query, set, disable, `/status`, `/forward`, and API against the live stick.

## Success criteria

- `/status` shows on / off / unavailable correctly without breaking SMS status.
- `/forward` can set from contact or typed number and disable without accidentally sending SMS.
- `GET`/`PUT /api/v1/call-forward` match the network state after operations.
- HTTP `/api/v1/status` schema and behavior unchanged.
