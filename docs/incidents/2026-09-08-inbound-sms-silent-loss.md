# Incident 2026-09-08 — Inbound SMS silently lost after host reboot

## Summary

After a host reboot on 2026-09-07 (~09:57 UTC), every incoming SMS was
received by the modem, then **silently deleted by the bot within seconds**
— no Telegram post, no `inbound_log` row, no error. Outbound SMS kept
working. The failure was completely silent: `today_in=0` with zero log
errors.

## Root cause

`Db::seen_sms()` deduplicated inbound SMS by **ModemManager D-Bus object
path first** (`inbound_log.mm_path`, e.g. `/org/freedesktop/ModemManager1/SMS/17`).

ModemManager numbers SMS paths from `/org/freedesktop/ModemManager1/SMS/0`
at every **restart**. After the reboot, new messages got low path numbers
(SMS/3 … SMS/22) that collided with `inbound_log` rows recorded weeks
earlier (Aug 20, after a previous restart). `seen_sms()` returned "seen"
for brand-new messages, `handle_incoming()` returned `Ok` without
recording or posting, and `handle_incoming_then_delete()` then deleted the
message from the modem.

The doc comment above `seen_sms` even warned that content (number + body +
modem timestamp) is the stable key — the path fast-path contradicted it.

### Why it looked like "everything" was broken

- Inbound: every message silently dropped (see above).
- Outbound: actually worked the whole time (verified via API and via a
  user-typed message that was delivered and 👍-acked). The dead inbound
  path made the bot appear dead.
- Three carrier multipart SMS (Ewano ref 150, HAMRAHAVAL, HAMRAH_AVAL)
  sat stuck in `receiving` state with empty text (missing parts never
  arrived). They occupied **27 of 50 SIM slots**, produced a
  "defer inbound sms until text is decoded" log line every sweep, and
  slowed each inbox sweep by ~15 s (5 s retry timeout each). The old
  sweep only purged them after 30 days.

## Fix

1. `src/db.rs` — `seen_sms()` no longer matches by path; dedup is content
   only (`e164 + body + sms_ts`). Messages with an empty modem timestamp
   are never treated as seen (a duplicate post is preferred over a lost
   message).
2. `src/app.rs` — `sweep_action()` deletes inbound SMS whose text is still
   empty (incomplete multipart) after `STUCK_RECEIVING_MAX` (6 h). The
   modem sweep (every 5 s processing, `sweep_old_sms` cycle) now self-heals
   stuck carrier messages instead of keeping them for 30 days.

Within seconds of deploying the fixed image, the sweep deleted the three
stuck messages and SIM storage went from 27/50 to 0/50 used.

## Verification

- `cargo test --lib` — 238 tests pass, including new regression tests:
  - `seen_sms_path_reuse_after_mm_restart_is_not_seen`
  - `seen_sms_empty_ts_is_never_seen`
  - `sweep_action_table` stuck-receiving cases
- Live loopback probe (SMS to the SIM's own number) was received,
  recorded (`inbound_log` row 212, `/org/freedesktop/ModemManager1/SMS/22`)
  and posted to the Telegram topic within ~20 s.
- `GET /api/v1/status` showed `today_in: 1`, `last_in: just now`.

## Detection in the future

`scripts/sms-health-check.sh` performs the same end-to-end loopback probe
and alerts via Telegram if a message does not come back. Run it from cron
(see the script header). A silent pipeline failure of this class shows up
as "probe not recorded" instead of nothing.

## Follow-up 2026-09-08 — path collision caused infinite redelivery

The content-only `seen_sms()` fix exposed a schema mismatch:
`inbound_log.mm_path` is `UNIQUE`, but paths are recycled slot ids after
every ModemManager restart. A multipart Snapp promo landed on
`/org/freedesktop/ModemManager1/SMS/27`, a path already held by an
unrelated Aug 20 row. Per sweep (5 s): `seen_sms` said "new" (content
differs) → the bot posted to Telegram → `record_inbound` hit
`UNIQUE(mm_path)` → error → message not deleted → reposted. The message
was delivered to Telegram every 5 seconds until the next fix.

Fix: `record_inbound()` upserts on `mm_path` conflict
(`ON CONFLICT DO UPDATE`), refreshing the stale row to the new message.
The dedup key remains content; the path column now means "the message
currently associated with this modem slot".

Regression tests: `record_inbound_recycled_path_replaces_stale_row`,
`incoming_new_message_on_recycled_path_delivers_once_and_deletes`.

## Unrelated finding — Google token revoked

Contacts sync has been failing since 2026-09-07 with
`invalid_grant: Token has been expired or revoked` (Google OAuth tokens
expire after 7 days when the app is in "testing" mode). Re-authenticate
with `cargo run -- auth` (or `docker compose run --rm telesms auth`) in a
browser session. Until then `contacts_ok: false` and new unknown numbers
land in General instead of named topics.
