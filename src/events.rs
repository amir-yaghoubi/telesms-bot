//! The application's realtime announcements.
//!
//! The application layer decides *when* a message is announced; this module
//! carries the announcement to whoever is listening. The persistence layer
//! knows nothing about it: it records a message and hands the row back, and
//! the layer that understood the message publishes it.

use tokio::sync::broadcast;

use crate::db::MessageRow;

/// How many announcements the bus buffers per subscriber.
pub const EVENT_BUFFER: usize = 256;

/// A message the gateway announces to realtime subscribers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MessageEvent {
    /// Message id in the API's format: `in:<rowid>` or `out:<rowid>`.
    pub id: String,
    /// `in` for a received SMS, `out` for a sent SMS.
    pub direction: String,
    pub e164: String,
    pub body: String,
    /// RFC 3339 time the gateway recorded the message.
    pub timestamp: String,
    /// Modem-side timestamp, when the modem reported one.
    pub sms_ts: Option<String>,
    /// `ok` or `failed`.
    pub status: String,
    pub thread_id: i32,
}

impl From<MessageRow> for MessageEvent {
    fn from(row: MessageRow) -> Self {
        Self {
            id: row.id,
            direction: row.direction,
            e164: row.e164,
            body: row.body,
            timestamp: row.timestamp,
            sms_ts: row.sms_ts,
            status: row.status,
            thread_id: row.thread_id,
        }
    }
}

/// Fan-out of announcements to every subscriber.
///
/// Delivery is fire-and-forget: a subscriber that cannot keep up misses
/// announcements instead of slowing the gateway down. Subscribers that care
/// about gaps resynchronize with a cursor.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<MessageEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Announce a message to every subscriber.
    pub fn publish(&self, event: MessageEvent) {
        // No subscribers is the normal case for an idle gateway.
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<MessageEvent> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str) -> MessageEvent {
        MessageEvent {
            id: id.into(),
            direction: "in".into(),
            e164: "+989121234567".into(),
            body: "hello".into(),
            timestamp: "2026-10-20T10:00:00+00:00".into(),
            sms_ts: None,
            status: "ok".into(),
            thread_id: 1,
        }
    }

    #[tokio::test]
    async fn subscriber_receives_published_messages() {
        let bus = EventBus::new(8);
        let mut rx = bus.subscribe();
        bus.publish(event("in:1"));
        let message = rx.recv().await.expect("message");
        assert_eq!(message.id, "in:1");
        assert_eq!(message.direction, "in");
    }

    #[tokio::test]
    async fn publish_without_subscribers_is_not_an_error() {
        let bus = EventBus::new(8);
        bus.publish(event("out:2"));
    }

    #[tokio::test]
    async fn every_subscriber_sees_the_same_message() {
        let bus = EventBus::new(8);
        let mut first = bus.subscribe();
        let mut second = bus.subscribe();
        bus.publish(event("in:3"));
        assert_eq!(first.recv().await.unwrap().id, "in:3");
        assert_eq!(second.recv().await.unwrap().id, "in:3");
    }

    #[tokio::test]
    async fn a_recorded_row_becomes_an_announcement() {
        let row = MessageRow {
            id: "in:7".into(),
            direction: "in".into(),
            e164: "+989121234567".into(),
            body: "hello".into(),
            timestamp: "2026-10-20T10:00:00+00:00".into(),
            sms_ts: Some("2026-10-20T09:59:58+00:00".into()),
            status: "ok".into(),
            thread_id: 9,
        };
        let event = MessageEvent::from(row);
        assert_eq!(event.id, "in:7");
        assert_eq!(event.thread_id, 9);
        assert_eq!(event.sms_ts.as_deref(), Some("2026-10-20T09:59:58+00:00"));
    }
}
