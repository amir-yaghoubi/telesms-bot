# 0002. Sort General-bound SMS before posting; OTP codes by learned patterns

Date: 2026-10-20
Status: Accepted

## Context

Most inbound SMS come from numbers with no contact and land in General: ads, operator broadcasts,
surveys, one-time codes and the occasional scam, all with the same loud notification. A local
SystemOne-protocol server (e.g. `mizan-v2-4b`) can tell these apart well enough (see the
`system-one-play` experiment), but it is a separate, optional process that may be slow or down.
Telegram cannot move a posted message to another forum topic, so whatever decides a message's
topic has to decide before the message is posted. The terminal client already extracts OTP codes
with user-taught regex patterns, locally and by polling.

## Decision

Add an opt-in **Classifier** (enabled by `CLASSIFIER_URL`) asked about every inbound SMS: one
**Category** with a confidence, and the `scam` **Tag**. It is consulted synchronously, before the
Telegram post, with a short per-attempt timeout, a few retries and a circuit breaker. If it does
not answer, the message is posted unclassified and a background sweep classifies it later, in the
database only.

Only a message that would land in General is **Sorted** into a dedicated **Sorted Thread**
(precedence `scam` > `otp` > low confidence stays in General > Category). Contact threads are never
re-routed: who sent a message matters more than what it is. Sorted Threads can be muted per Sort.

**OTP Codes** come only from **OTP Patterns** learned by the gateway from a sample message and the
code the user points at, never from the Classifier. Patterns live in the gateway's database, so
every client, Telegram included, shares one set and the terminal client stops keeping its own.

Classifications and OTP codes are stored on the message and exposed by the HTTP API and the
WebSocket; a later change to a recorded message is announced as `message.updated`.

## Consequences

- A slow or dead Classifier delays inbound posts by up to the retry budget until the breaker
  opens; delivery itself never depends on it.
- A late classification never moves the Telegram post; only the stored message, API and WebSocket
  see it.
- OTP extraction is deterministic and works with the Classifier switched off; unknown formats are
  missed until the user teaches a pattern.
- Messages recorded before this feature are classified by an offline backfill command that writes
  the database only and announces nothing (consistent with ADR 0001).

## Alternatives considered

- **Post first, classify after** — rejected: the message would be stuck in General, defeating the
  point of Sorted Threads and muting.
- **Block delivery until classified** — rejected: the gateway's job is delivery; an optional
  helper must not be able to stop it.
- **Classifier detects OTPs (`has_otp`, `otp` category)** — rejected: a probabilistic answer cannot
  produce the code itself, and learned patterns already proved reliable in the terminal client.
- **Re-route contact threads too** — rejected: a contact's OTP or bill belongs in that contact's
  conversation.
