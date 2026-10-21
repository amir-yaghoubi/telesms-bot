# telesms-bot — context

This is the domain glossary for the gateway. It contains no implementation detail; see
[README.md](README.md) for behaviour and [docs/adr/](docs/adr/) for hard-to-reverse decisions.

## Purpose

The **Gateway** turns a modem and a Telegram forum into a personal **SMS gateway**. It owns the
messages, the contacts, the routing of a message to a conversation, and the two ways a client can
talk to it: request/response over HTTP and a live stream over a WebSocket.

## Language

**Gateway**:
The telesms-bot daemon and its modem. Owns SMS, contacts, history and Telegram forum topics.
_Avoid_: Bot, server, backend

**Client**:
Any consumer of the gateway: the terminal client, the desktop client, or a program holding a
WebSocket connection. A client never owns messages; it reads them from the gateway.
_Avoid_: App, user

**Thread**:
A Telegram forum topic identified by `thread_id`. The conversation a message belongs to.
_Avoid_: Topic, chat, room

**General**:
The special thread `thread_id = 1`, collecting SMS from numbers without a dedicated topic. It has
no default number of its own.
_Avoid_: Inbox, default chat

**Contact**:
A Google contact known to the gateway, identified by `contact_id`; may have several numbers.
_Avoid_: Person, account

**Number**:
A phone number, normally E.164, that can be an identity on its own.
_Avoid_: Phone, e164

**Identity**:
The triple `number`, `contact_id`, `thread_id`. Routes that resolve an entity require `number` or
`contact_id`; a `thread_id` alone is not an identity.
_Avoid_: Target, addressee

**Message**:
One recorded SMS: direction (`in`/`out`), E.164, body, timestamp, optional modem time, and status
(`ok`/`failed`). A message exists only once the gateway has recorded it.
_Avoid_: SMS event, frame, notification

**Event**:
A live notification that names one recorded Message. An event is not a message: it is how a
message reaches a subscriber.
_Avoid_: Message, push, notification

**Command**:
A request a client sends over the live channel, answered by an acknowledgement or an error.
_Avoid_: Request, action, RPC

**Cursor**:
A timestamp a client hands to the gateway to ask for the messages recorded after it.
_Avoid_: Offset, page, since

**Replay**:
The delivery of the messages a client missed, fetched by cursor when it reconnects.
_Avoid_: Backfill, history, catch-up

**Classifier**:
The optional external decision maker (a SystemOne-protocol server) the gateway asks what kind of
SMS an inbound message is. The gateway works fully without it.
_Avoid_: AI, model, LLM, spam filter

**Classification**:
The classifier's verdict on one inbound Message: exactly one Category, its confidence, and zero or
more Tags. Outbound messages never have one; an inbound message may have none yet.
_Avoid_: Label, prediction, result

**Category**:
What an inbound SMS is from the recipient's point of view: `transactional`, `promotional`,
`broadcast`, `feedback`, `personal` or `other`. One per Classification. One-time codes are not a
Category; they are found by OTP Patterns.
_Avoid_: Type, kind, label

**Tag**:
A yes/no property of an inbound SMS independent of its Category, currently only `scam`
(impersonation or phishing). A message may carry several.
_Avoid_: Flag, label, category

**Sort**:
The gateway's decision about where a message that would land in General goes instead: `scam`,
`otp`, one Category, or none (stays in General). Decided in that order of precedence; a
low-confidence Classification sorts to none.
_Avoid_: Label, bucket, route

**Sorted Thread**:
A Thread the gateway creates for one Sort and fills with messages that would otherwise land in
General. Like General, it has no reply address of its own.
_Avoid_: Category thread, ads topic, spam folder, bin

**OTP Code**:
The one-time code the gateway extracts from an inbound message body with an OTP Pattern. Found
without the Classifier.
_Avoid_: Password, PIN, token

**OTP Pattern**:
A learned rule that locates the OTP Code in a message body. The gateway learns one when the user
points at the code in a sample message.
_Avoid_: Regex, rule, template

## Relationships

- A **Gateway** owns many **Threads**, **Contacts** and **Messages**.
- A **Thread** contains many **Messages**, which may come from more than one **Number**.
- An **Identity** may resolve to a **Contact**, a **Number**, a **Thread**, or a combination.
- Recording a **Message** is what the gateway announces as an **Event** to every client on the live
  channel.
- A **Command** is answered by an acknowledgement or an error, never by an **Event**.
- A **Cursor** turns a reconnection into a **Replay**.
- An inbound **Message** has at most one **Classification**; a **Classification** has one
  **Category** and any number of **Tags**.
- Only a message that would land in **General** gets a **Sort**; a message for a contact's thread
  stays there whatever its **Classification** or **OTP Code**.
- A **Sorted Thread** belongs to exactly one **Sort**.
- An **OTP Pattern** yields an **OTP Code**; the **Classifier** never does.

## Invariants

- General cannot be resolved on its own and has no reply address; replies target a message's number.
- A thread may contain messages from multiple numbers; the per-message number must stay visible.
- A contact with multiple numbers and no default cannot be sent to without choosing a number.
- A message is recorded before it is announced; a client never sees a message the gateway has not
  recorded.
- Only the layer that understood a message announces it; storage alone never speaks to a client.
- The live channel requires the same credential as the HTTP API.
