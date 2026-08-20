# SMS Send Status via Telegram Reactions

Date: 2026-08-20

When the owner sends an SMS from Telegram (topic text or `/sms`), replace the success **text reply** (`✅`) with **emoji reactions** on the owner’s message. Show progress while the modem is working, then outcome. Failed sends still include the error text so diagnosis is not lost.

## Goals

- React `📨` on the owner message as soon as send starts.
- Replace with `✅` on success (no success text reply).
- Replace with `❌` on failure **and** reply with the error text.
- Keep text-based ack when there is no message to react to (e.g. HTTP API with `reply_to = None`).

## Non-goals

- Changing HTTP API response bodies or OpenAPI.
- Custom emoji / paid reactions.
- Stacking multiple reactions (Telegram replaces the bot’s reaction set).
- Reacting on inbound SMS delivery messages.
- Showing modem progress beyond the three status emojis.

## Approach

**Extend `TelegramSink` with `react`** (Approach 1).

`send_and_ack` owns the lifecycle so every Telegram send path (`handle_owner_text`, `/sms`, pending-number callback) stays consistent.

Rejected alternatives:

- **Reactions only from Telegram handlers:** splits ack logic; easy to miss a call site.
- **Temporary “sending…” status message:** more chat noise; not what we want.

## Behavior

| Case | Behavior |
|------|----------|
| `reply_to` present | `react(📨)` → modem send → `react(✅)` on success; on failure `react(❌)` + text reply with error |
| `reply_to` absent | No reactions; success → post `✅`; failure → post error text (current fallback) |

Constants (names may vary slightly in code):

- `SEND_PENDING` = `📨`
- `SEND_ACK` = `✅` (existing)
- `SEND_FAIL` = `❌`

## Architecture

```
Telegram owner message (message_id)
        │
        ▼
  send_and_ack
        ├── react(📨)          if reply_to
        ├── modem.send
        ├── react(✅)          success + reply_to
        ├── post(✅)           success + no reply_to
        ├── react(❌)+reply    failure + reply_to
        └── post(error)        failure + no reply_to
```

### Components

1. **`TelegramSink::react(message_id, emoji)`**
   - `RealTg`: `bot.set_message_reaction(chat_id, MessageId).reaction([ReactionType::Emoji { emoji }])`
   - `FakeTg`: append to `reactions: Mutex<Vec<(i32, String)>>` for assertions
2. **`send_and_ack`** in `app.rs` — single place for pending / success / fail ack
3. **No DB schema changes**; `pending_reply_to` continues to carry the message id through the “which number?” flow

## Error handling

| Stage | Policy |
|-------|--------|
| `📨` fails before modem send | Abort; return `TelegramFailed { sent: false }`; SMS not attempted |
| Modem fails | Best-effort `❌` (+ error reply when `reply_to`); return `ModemFailed` (ack errors on fail still ignored, as today) |
| Success but final `✅` fails | `TelegramFailed { sent: true }` |

Reactions replace the previous bot reaction on that message (normal Bot API behavior).

## Testing

Update existing `send_and_ack` / action tests:

- With `reply_to`: reaction sequence `📨` → `✅`; no success text reply/post.
- With `reply_to` + modem error: `📨` → `❌`; error text reply present.
- Without `reply_to`: no reactions; text `✅` / error post as today.
- Optional: `📨` react fails → modem never called.

## Out of scope for follow-ups

- Documenting reaction UX in README (can be a small docs note in the same PR if desired).
- Group-level `available_reactions` restrictions (assume standard emoji reactions are allowed in the bot’s forum group).
