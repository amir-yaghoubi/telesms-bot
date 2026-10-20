# 0001. Opt-in WebSocket server for realtime messages

Date: 2026-10-20
Status: Accepted

## Context

The API is read-mostly with cursor pagination and no push: clients learn about a new SMS by
polling (`/api/v1/chats`, `/api/v1/chats/{id}/messages`) every 3–30 seconds. The gateway owns the
only two paths that produce a message — the application layer handling an inbound SMS
(`handle_incoming`) and the application layer sending an outbound one (`send_and_ack`) — so it is
the only place a realtime stream can be produced at true event time. The gateway is single-user:
every client is entitled to every message, so there is nothing to filter.

## Decision

Add a second transport next to the REST API: a standalone `tokio-tungstenite` listener on its own
port, started only when `WS_ENABLED=true` and an API key is configured. It authenticates with the
same key (`X-Api-Key` header, `?api_key=` fallback for browsers) and speaks a versioned JSON frame
protocol: **events** for recorded messages (`message.received`, `message.sent`, `message.failed`) and
exactly one **command**, `send_sms`, answered by an acknowledgement or an error carrying the same
error vocabulary as the HTTP API.

The **application layer** announces a message, not the database. The database records a message and
returns the row it wrote; the layer that understood the message — which knows its direction, its
thread and whether the send succeeded — announces it on the event bus. The persistence layer knows
nothing about realtime, and the transports never decide on their own that something happened.

Delivery is fire-and-forget with no subscription filtering. A client that cannot keep up is
disconnected rather than allowed to slow the gateway down, and resynchronizes by reconnecting with
`?after=<cursor>`, which replays the messages it missed. The server pings every 30 seconds and
closes a client that has sent nothing within 60.

## Consequences

- Two ports to expose, and a deployment that wants realtime has to enable and publish the second
  one.
- Clients that use the live channel must implement reconnection and cursor resynchronization; the
  REST API stays the way to read history.
- The recorded message is the unit of both transports, so a payload never needs a second
  interpretation.
- The application layer owns the announcement, so a write that bypasses it (a migration, an
  administrative script) produces no event — the announcement belongs with the decision, not with
  the write.
- The desktop client's "no push" premise ([telesms-desktop ADR 0003]) stays true until that client
  migrates.

## Alternatives considered

- **Upgrade on the API port (`/api/v1/ws`)** — rejected: it makes the live channel part of the
  API's surface, so it can no longer be switched off per deployment, and the axum `ws` feature
  would have hidden the `tokio-tungstenite` dependency the design asked for.
- **Server-sent events** — rejected: one-way only, and clients need to ask for a message to be sent.
- **Long polling** — rejected: it keeps the polling load and the per-request latency this feature
  exists to remove.
- **Per-thread subscriptions** — rejected: a single-user gateway has nothing to hide, and the
  protocol would grow a subscribe command and per-connection state for no benefit.
