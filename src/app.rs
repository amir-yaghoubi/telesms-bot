use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::db::{Db, Topic};
use crate::modem::{IncomingSms, ModemError, ModemInfo, SmsInbox, SmsModem};
use crate::actions::ActionError;
use crate::modem_mm::MmModem;
use crate::normalize::normalize_e164;
use crate::route::{plan_outbound, route_for_send, route_inbound, InboundDest, OutboundPlan, GENERAL_THREAD};

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Db(#[from] crate::db::DbError),
    #[error(transparent)]
    Modem(#[from] ModemError),
    #[error("telegram: {0}")]
    Telegram(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceEvent {
    Offline,
    Back,
}

/// Tracks modem present/absent transitions. Starts assumed present so the
/// first absence posts offline once and a later return posts back once.
pub struct Presence {
    last: bool,
}

impl Presence {
    pub fn new() -> Self {
        Self { last: true }
    }

    pub fn observe(&mut self, present: bool) -> Option<PresenceEvent> {
        let was = self.last;
        self.last = present;
        match (was, present) {
            (true, false) => Some(PresenceEvent::Offline),
            (false, true) => Some(PresenceEvent::Back),
            _ => None,
        }
    }
}

pub async fn watch_modem(
    info: Arc<dyn ModemInfo>,
    tg: Arc<dyn TelegramSink>,
    interval: Duration,
    cancel: CancellationToken,
) {
    let mut presence = Presence::new();
    loop {
        let present = info.snapshot().await.is_ok();
        let text = match presence.observe(present) {
            Some(PresenceEvent::Offline) => Some("modem offline"),
            Some(PresenceEvent::Back) => Some("modem back"),
            None => None,
        };
        if let Some(text) = text {
            if let Err(err) = tg.post(GENERAL_THREAD, text.to_string()).await {
                tracing::error!(error = %err, "failed to post modem presence");
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

pub async fn watch_inbox<I>(
    inbox: Arc<I>,
    db: Arc<Db>,
    region: String,
    tg: Arc<dyn TelegramSink>,
    delete_enabled: bool,
    cancel: CancellationToken,
    retry: Duration,
) where
    I: SmsInbox + 'static,
{
    loop {
        if cancel.is_cancelled() {
            return;
        }
        match inbox.subscribe_added().await {
            Ok(stream) => {
                drain_listed_sms(
                    inbox.as_ref(),
                    db.as_ref(),
                    &region,
                    tg.as_ref(),
                    delete_enabled,
                    &cancel,
                )
                .await;
                let mut stream = std::pin::pin!(stream);
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(retry) => {
                            drain_listed_sms(
                                inbox.as_ref(),
                                db.as_ref(),
                                &region,
                                tg.as_ref(),
                                delete_enabled,
                                &cancel,
                            )
                            .await;
                        }
                        sms = stream.next() => {
                            let Some(sms) = sms else {
                                tracing::warn!("modem added signal stream ended");
                                break;
                            };
                            if let Err(err) = handle_incoming_then_delete(
                                &db,
                                &region,
                                sms,
                                tg.as_ref(),
                                inbox.as_ref(),
                                delete_enabled,
                            )
                            .await
                            {
                                tracing::error!(error = %err, "incoming sms");
                            }
                        }
                    }
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "subscribe modem added failed");
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(retry) => {}
        }
    }
}

pub const SEND_PENDING: &str = "👀";
pub const SEND_ACK: &str = "✅";
pub const SEND_REACT_OK: &str = "👍";
pub const SEND_FAIL: &str = "👎";

#[async_trait::async_trait]
pub trait TelegramSink: Send + Sync {
    async fn post(&self, thread_id: i32, text: String) -> Result<(), AppError>;
    async fn reply(&self, thread_id: i32, text: String, _reply_to: i32) -> Result<(), AppError> {
        self.post(thread_id, text).await
    }
    async fn react(&self, _message_id: i32, _emoji: &str) -> Result<(), AppError> {
        Err(AppError::Telegram("react not supported".into()))
    }
    async fn create_topic(&self, title: String) -> Result<i32, AppError>;
}

pub struct FakeTg {
    pub posts: Mutex<Vec<(i32, String)>>,
    pub replies: Mutex<Vec<(i32, String, i32)>>,
    pub reactions: Mutex<Vec<(i32, String)>>,
    pub next_thread: AtomicI32,
    pub fail: bool,
}

impl FakeTg {
    pub fn new() -> Self {
        Self {
            posts: Mutex::new(Vec::new()),
            replies: Mutex::new(Vec::new()),
            reactions: Mutex::new(Vec::new()),
            next_thread: AtomicI32::new(100),
            fail: false,
        }
    }
}

impl Default for FakeTg {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl TelegramSink for FakeTg {
    async fn post(&self, thread_id: i32, text: String) -> Result<(), AppError> {
        if self.fail {
            return Err(AppError::Telegram("fail".into()));
        }
        self.posts
            .lock()
            .expect("fake tg posts lock")
            .push((thread_id, text));
        Ok(())
    }

    async fn reply(&self, thread_id: i32, text: String, reply_to: i32) -> Result<(), AppError> {
        if self.fail {
            return Err(AppError::Telegram("fail".into()));
        }
        self.replies
            .lock()
            .expect("fake tg replies lock")
            .push((thread_id, text, reply_to));
        Ok(())
    }

    async fn react(&self, message_id: i32, emoji: &str) -> Result<(), AppError> {
        if self.fail {
            return Err(AppError::Telegram("fail".into()));
        }
        self.reactions
            .lock()
            .expect("fake tg reactions lock")
            .push((message_id, emoji.to_string()));
        Ok(())
    }

    async fn create_topic(&self, _title: String) -> Result<i32, AppError> {
        if self.fail {
            return Err(AppError::Telegram("fail".into()));
        }
        Ok(self.next_thread.fetch_add(1, Ordering::SeqCst))
    }
}

async fn drain_listed_sms<I>(
    inbox: &I,
    db: &Db,
    region: &str,
    tg: &dyn TelegramSink,
    delete_enabled: bool,
    cancel: &CancellationToken,
) where
    I: SmsInbox,
{
    match inbox.list_sms().await {
        Ok(existing) => {
            if !existing.is_empty() {
                tracing::info!(n = existing.len(), "processing modem inbox");
            }
            for sms in existing {
                if cancel.is_cancelled() {
                    return;
                }
                if let Err(err) =
                    handle_incoming_then_delete(db, region, sms, tg, inbox, delete_enabled).await
                {
                    tracing::error!(error = %err, "existing sms");
                }
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "list modem inbox failed");
        }
    }
}

pub async fn maybe_delete(enabled: bool, modem: &dyn SmsModem, path: &str) {
    if !enabled || path.is_empty() {
        return;
    }
    if let Err(err) = modem.delete(path).await {
        tracing::warn!(path, error = %err, "sms delete failed");
    }
}

pub async fn handle_incoming_then_delete(
    db: &Db,
    region: &str,
    sms: IncomingSms,
    tg: &dyn TelegramSink,
    modem: &dyn SmsModem,
    delete_enabled: bool,
) -> Result<(), AppError> {
    if sms.inbound && sms.text.is_empty() {
        tracing::info!(
            path = %sms.path,
            e164 = %sms.e164,
            "defer inbound sms until text is decoded"
        );
        return Ok(());
    }
    let path = sms.path.clone();
    match handle_incoming(db, region, sms, tg).await {
        Ok(()) => {
            maybe_delete(delete_enabled, modem, &path).await;
            Ok(())
        }
        Err(err) => Err(err),
    }
}

pub async fn handle_incoming(
    db: &Db,
    region: &str,
    sms: IncomingSms,
    tg: &dyn TelegramSink,
) -> Result<(), AppError> {
    if !sms.inbound {
        return Ok(());
    }
    let id_e164 = normalize_e164(&sms.e164, region).unwrap_or_else(|_| sms.e164.clone());
    if db.seen_sms(&sms.path, &id_e164, &sms.text, &sms.timestamp)? {
        return Ok(());
    }
    if sms_too_old(&sms.timestamp) {
        tracing::info!(
            path = %sms.path,
            ts = %sms.timestamp,
            "skip stale inbound sms"
        );
        let thread_id = match route_for_send(db, &id_e164)? {
            InboundDest::ExistingTopic { thread_id, .. } => thread_id,
            InboundDest::CreateContactTopic { .. } | InboundDest::General { .. } => GENERAL_THREAD,
        };
        db.record_inbound(
            &sms.path,
            &id_e164,
            &sms.text,
            None,
            &sms.timestamp,
            thread_id,
        )?;
        return Ok(());
    }

    match deliver_incoming(db, region, &sms, tg).await {
        Ok(Delivered::Normalized { e164, thread_id }) => {
            db.mark_incoming(&e164)?;
            db.record_inbound(
                &sms.path,
                &e164,
                &sms.text,
                None,
                &sms.timestamp,
                thread_id,
            )?;
            Ok(())
        }
        Ok(Delivered::Raw { thread_id }) => {
            db.record_inbound(
                &sms.path,
                &id_e164,
                &sms.text,
                None,
                &sms.timestamp,
                thread_id,
            )?;
            Ok(())
        }
        Err(err) => {
            if matches!(err, AppError::Telegram(_)) {
                tracing::error!(
                    e164 = %sms.e164,
                    text = %sms.text,
                    error = %err,
                    "telegram failed; inbound sms not marked seen"
                );
            }
            Err(err)
        }
    }
}

pub(crate) fn parse_sms_timestamp(ts: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    if ts.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        return Some(dt);
    }
    // ModemManager sometimes emits +03 instead of +03:00
    chrono::DateTime::parse_from_rfc3339(&format!("{ts}:00")).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepAction {
    Keep,
    Delete,
}

fn sms_age_duration(ts: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let dt = parse_sms_timestamp(ts)?;
    let delta = now.signed_duration_since(dt.with_timezone(&chrono::Utc));
    let secs = delta.num_seconds().max(0) as u64;
    Some(Duration::from_secs(secs))
}

pub fn sweep_action(
    sms: &IncomingSms,
    seen: bool,
    now: chrono::DateTime<chrono::Utc>,
    max_age: Duration,
    inbound_retry_window: Duration,
) -> SweepAction {
    let age = sms_age_duration(&sms.timestamp, now);
    if sms.inbound && !seen {
        return match age {
            None => SweepAction::Keep,
            Some(age) if age < inbound_retry_window => SweepAction::Keep,
            Some(age) if age <= max_age => SweepAction::Keep,
            Some(_) => SweepAction::Delete,
        };
    }
    match age {
        None => SweepAction::Delete,
        Some(age) if age > max_age => SweepAction::Delete,
        Some(_) => SweepAction::Keep,
    }
}

pub async fn sweep_old_sms(modem: &MmModem, db: &Db, max_age: Duration) -> Result<(), ModemError> {
    let now = chrono::Utc::now();
    let window = Duration::from_secs(24 * 3600);
    let list = modem.list_sms().await?;
    for sms in list {
        let seen = match db.seen_sms(&sms.path, &sms.e164, &sms.text, &sms.timestamp) {
            Ok(seen) => seen,
            Err(err) => {
                tracing::warn!(path = %sms.path, error = %err, "sweep seen check failed");
                continue;
            }
        };
        if sweep_action(&sms, seen, now, max_age, window) != SweepAction::Delete {
            continue;
        }
        if let Err(err) = SmsModem::delete(modem, &sms.path).await {
            tracing::warn!(path = %sms.path, error = %err, "sweep delete failed");
        }
    }
    Ok(())
}

fn sms_too_old(ts: &str) -> bool {
    let Some(dt) = parse_sms_timestamp(ts) else {
        return false;
    };
    chrono::Utc::now().signed_duration_since(dt.with_timezone(&chrono::Utc))
        > chrono::TimeDelta::hours(24)
}

enum Delivered {
    Normalized { e164: String, thread_id: i32 },
    Raw { thread_id: i32 },
}

async fn deliver_incoming(
    db: &Db,
    region: &str,
    sms: &IncomingSms,
    tg: &dyn TelegramSink,
) -> Result<Delivered, AppError> {
    let e164 = match normalize_e164(&sms.e164, region) {
        Ok(e164) => e164,
        Err(_) => {
            tg.post(GENERAL_THREAD, format!("{}\n{}", sms.e164, sms.text))
                .await?;
            return Ok(Delivered::Raw {
                thread_id: GENERAL_THREAD,
            });
        }
    };

    let thread_id = match route_inbound(db, &e164)? {
        InboundDest::CreateContactTopic {
            contact_id,
            title,
            default_e164,
        } => {
            let thread_id = tg.create_topic(title.clone()).await?;
            db.upsert_topic(&Topic {
                thread_id,
                contact_id: Some(contact_id),
                default_e164: Some(default_e164),
                title,
                ignored: false,
            })?;
            tg.post(thread_id, sms.text.clone()).await?;
            thread_id
        }
        InboundDest::ExistingTopic {
            thread_id,
            note_switch_to,
        } => {
            if let Some(switched) = note_switch_to {
                tg.post(thread_id, format!("now using {switched}")).await?;
            }
            tg.post(thread_id, sms.text.clone()).await?;
            thread_id
        }
        InboundDest::General { e164: dest_e164 } => {
            tg.post(GENERAL_THREAD, format!("{dest_e164}\n{}", sms.text))
                .await?;
            GENERAL_THREAD
        }
    };

    Ok(Delivered::Normalized { e164, thread_id })
}

#[derive(Debug, PartialEq, Eq)]
pub enum OwnerTextOutcome {
    Done,
    NeedNumber(Vec<String>),
}

async fn ack_send(
    tg: &dyn TelegramSink,
    thread_id: i32,
    text: &str,
    reply_to: Option<i32>,
) -> Result<(), AppError> {
    if let Some(id) = reply_to {
        tg.reply(thread_id, text.to_string(), id).await
    } else {
        tg.post(thread_id, text.to_string()).await
    }
}

pub async fn send_and_ack(
    db: &Db,
    e164: &str,
    text: &str,
    thread_id: i32,
    reply_to: Option<i32>,
    modem: &dyn SmsModem,
    tg: &dyn TelegramSink,
    delete_enabled: bool,
) -> Result<(), ActionError> {
    if let Some(id) = reply_to {
        if let Err(err) = tg.react(id, SEND_PENDING).await {
            return Err(ActionError::TelegramFailed {
                sent: false,
                message: err.to_string(),
            });
        }
    }

    match modem.send(e164, text).await {
        Ok(path) => {
            db.record_outbound(e164, text, "ok", thread_id)?;
            let ack_result = match reply_to {
                Some(id) => tg.react(id, SEND_REACT_OK).await,
                None => ack_send(tg, thread_id, SEND_ACK, None).await,
            };
            if let Err(err) = ack_result {
                return Err(ActionError::TelegramFailed {
                    sent: true,
                    message: err.to_string(),
                });
            }
            maybe_delete(delete_enabled, modem, &path).await;
            Ok(())
        }
        Err(err) => {
            let err_s = err.to_string();
            db.record_outbound(e164, text, &err_s, thread_id)?;
            if let Some(id) = reply_to {
                let _ = tg.react(id, SEND_FAIL).await;
            }
            let _ = ack_send(tg, thread_id, &err_s, reply_to).await;
            Err(ActionError::ModemFailed(err_s))
        }
    }
}

pub async fn handle_owner_text(
    db: &Db,
    region: &str,
    thread_id: i32,
    text: &str,
    reply_to: Option<i32>,
    modem: &dyn SmsModem,
    tg: &dyn TelegramSink,
    delete_enabled: bool,
) -> Result<OwnerTextOutcome, AppError> {
    let _ = region;
    if text.starts_with('/') {
        return Ok(OwnerTextOutcome::Done);
    }
    match plan_outbound(db, thread_id)? {
        OutboundPlan::NotSms | OutboundPlan::UnknownTopic => Ok(OwnerTextOutcome::Done),
        OutboundPlan::AskWhichNumber { numbers, .. } => {
            db.set_pending_outbound(thread_id, text, reply_to)?;
            Ok(OwnerTextOutcome::NeedNumber(numbers))
        }
        OutboundPlan::Send { e164 } => {
            match send_and_ack(
                db,
                &e164,
                text,
                thread_id,
                reply_to,
                modem,
                tg,
                delete_enabled,
            )
            .await
            {
                Ok(()) => {}
                Err(ActionError::ModemFailed(_)) => {}
                Err(ActionError::Db(e)) => return Err(e.into()),
                Err(e) => return Err(AppError::Telegram(e.to_string())),
            }
            Ok(OwnerTextOutcome::Done)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::modem::FakeModem;

    #[tokio::test]
    async fn incoming_known_contact_creates_topic_and_posts_body() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        let tg = FakeTg::new();
        handle_incoming(
            &db,
            "IR",
            IncomingSms {
                path: "/sms/1".into(),
                e164: "09121234567".into(),
                text: "hi".into(),
                inbound: true,
                timestamp: chrono::Utc::now().to_rfc3339(),
            },
            &tg,
        )
        .await
        .unwrap();
        let posts = tg.posts.lock().unwrap();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].1, "hi");
        assert!(db.get_topic_by_contact(id).unwrap().is_some());
    }

    #[tokio::test]
    async fn duplicate_path_not_posted_twice() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        let tg = FakeTg::new();
        let sms = IncomingSms {
            path: "/sms/1".into(),
            e164: "09121234567".into(),
            text: "hi".into(),
            inbound: true,
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        handle_incoming(&db, "IR", sms.clone(), &tg).await.unwrap();
        handle_incoming(&db, "IR", sms, &tg).await.unwrap();
        assert_eq!(tg.posts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn same_content_new_path_not_posted_twice() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let ts = chrono::Utc::now().to_rfc3339();
        let a = IncomingSms {
            path: "/sms/1".into(),
            e164: "+989120000001".into(),
            text: "hi".into(),
            inbound: true,
            timestamp: ts.clone(),
        };
        let mut b = a.clone();
        b.path = "/sms/2".into();
        handle_incoming(&db, "IR", a, &tg).await.unwrap();
        handle_incoming(&db, "IR", b, &tg).await.unwrap();
        assert_eq!(tg.posts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stale_inbound_not_posted() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        handle_incoming(
            &db,
            "IR",
            IncomingSms {
                path: "/sms/old".into(),
                e164: "+989120000002".into(),
                text: "old".into(),
                inbound: true,
                timestamp: "2024-12-21T15:12:23+03:30".into(),
            },
            &tg,
        )
        .await
        .unwrap();
        assert!(tg.posts.lock().unwrap().is_empty());
        assert!(db.seen_sms_path("/sms/old").unwrap());
    }

    #[tokio::test]
    async fn stale_inbound_resolves_thread_without_switching_default() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        let a = "+989120000001";
        let b = "+989120000002";
        db.replace_contact_numbers(id, &[a.into(), b.into()])
            .unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: Some(id),
            default_e164: Some(a.into()),
            title: "Ali (0001)".into(),
            ignored: false,
        })
        .unwrap();
        let tg = FakeTg::new();
        handle_incoming(
            &db,
            "IR",
            IncomingSms {
                path: "/sms/stale-b".into(),
                e164: b.into(),
                text: "stale from B".into(),
                inbound: true,
                timestamp: "2024-12-21T15:12:23+03:30".into(),
            },
            &tg,
        )
        .await
        .unwrap();
        assert!(tg.posts.lock().unwrap().is_empty());
        assert_eq!(
            db.get_topic_by_thread(42)
                .unwrap()
                .unwrap()
                .default_e164
                .as_deref(),
            Some(a)
        );
        assert_eq!(db.inbound_thread_id("/sms/stale-b").unwrap(), Some(42));
    }

    #[tokio::test]
    async fn incoming_telegram_fail_does_not_mark_seen() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: Some(id),
            default_e164: Some("+989121234567".into()),
            title: "Ali (4567)".into(),
            ignored: false,
        })
        .unwrap();
        let tg = FakeTg {
            fail: true,
            ..FakeTg::new()
        };
        let sms = IncomingSms {
            path: "/sms/1".into(),
            e164: "09121234567".into(),
            text: "secret body".into(),
            inbound: true,
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        assert!(handle_incoming(&db, "IR", sms, &tg).await.is_err());
        assert!(!db.seen_sms_path("/sms/1").unwrap());
    }

    #[tokio::test]
    async fn owner_text_in_general_does_not_send() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        handle_owner_text(&db, "IR", 1, "hello", None, &modem, &tg, true)
            .await
            .unwrap();
        assert!(modem.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn owner_text_in_topic_sends_and_acks() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: Some(id),
            default_e164: Some("+989121234567".into()),
            title: "Ali (4567)".into(),
            ignored: false,
        })
        .unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        handle_owner_text(&db, "IR", 42, "hello", Some(7), &modem, &tg, true)
            .await
            .unwrap();
        let sent = modem.sent.lock().unwrap();
        assert_eq!(sent.as_slice(), &[("+989121234567".into(), "hello".into())]);
        assert_eq!(
            tg.reactions.lock().unwrap().as_slice(),
            &[(7, SEND_PENDING.into()), (7, SEND_REACT_OK.into())]
        );
        assert!(tg.replies.lock().unwrap().is_empty());
    }

    #[test]
    fn empty_timestamp_is_not_stale() {
        assert!(!sms_too_old(""));
    }

    #[test]
    fn plus_hh_offset_parses_as_stale() {
        assert!(sms_too_old("2024-11-27T19:10:49+03"));
    }

    #[test]
    fn modem_watch_emits_single_offline() {
        let mut w = Presence::new();
        assert_eq!(w.observe(false), Some(PresenceEvent::Offline));
        assert_eq!(w.observe(false), None);
        assert_eq!(w.observe(true), Some(PresenceEvent::Back));
    }

    #[tokio::test]
    async fn watch_modem_posts_offline_and_stops_on_cancel() {
        let tg = Arc::new(FakeTg::new());
        let modem: Arc<dyn ModemInfo> = Arc::new(FakeModem::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(watch_modem(
            modem,
            tg.clone(),
            Duration::from_millis(15),
            cancel.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(40)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("watch_modem did not stop")
            .expect("watch_modem join");
        assert_eq!(
            tg.posts.lock().unwrap().as_slice(),
            &[(GENERAL_THREAD, "modem offline".into())]
        );
    }

    #[tokio::test]
    async fn fake_tg_react_records_emoji() {
        let tg = FakeTg::new();
        tg.react(7, SEND_PENDING).await.unwrap();
        tg.react(7, SEND_REACT_OK).await.unwrap();
        assert_eq!(
            tg.reactions.lock().unwrap().as_slice(),
            &[(7, SEND_PENDING.into()), (7, SEND_REACT_OK.into())]
        );
    }

    #[test]
    fn send_status_emoji_match_telegram_reactions() {
        assert_eq!(SEND_PENDING, "👀");
        assert_eq!(SEND_REACT_OK, "👍");
        assert_eq!(SEND_FAIL, "👎");
        assert_eq!(SEND_ACK, "✅");
    }

    #[tokio::test]
    async fn send_and_ack_ok_reacts_and_deletes() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        send_and_ack(&db, "+98912", "hi", 42, Some(7), &modem, &tg, true)
            .await
            .unwrap();
        assert_eq!(
            modem.sent.lock().unwrap().as_slice(),
            &[("+98912".into(), "hi".into())]
        );
        assert_eq!(
            tg.reactions.lock().unwrap().as_slice(),
            &[(7, SEND_PENDING.into()), (7, SEND_REACT_OK.into())]
        );
        assert!(tg.replies.lock().unwrap().is_empty());
        assert!(tg.posts.lock().unwrap().is_empty());
        assert_eq!(
            modem.deleted.lock().unwrap().as_slice(),
            &["/fake/sms/1".into()] as &[String]
        );
        assert_eq!(db.last_outbound_ok().unwrap().unwrap().0, "+98912");
    }

    #[tokio::test]
    async fn send_and_ack_ok_without_reply_to_posts_ack() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        send_and_ack(&db, "+98912", "hi", 42, None, &modem, &tg, true)
            .await
            .unwrap();
        assert!(tg.reactions.lock().unwrap().is_empty());
        assert_eq!(
            tg.posts.lock().unwrap().as_slice(),
            &[(42, SEND_ACK.into())]
        );
    }

    #[tokio::test]
    async fn send_and_ack_err_reacts_fail_and_replies_error() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem {
            fail: true,
            ..FakeModem::default()
        };
        let err = send_and_ack(&db, "+98912", "hi", 42, Some(7), &modem, &tg, true)
            .await
            .unwrap_err();
        assert!(matches!(err, ActionError::ModemFailed(_)));
        assert_eq!(
            tg.reactions.lock().unwrap().as_slice(),
            &[(7, SEND_PENDING.into()), (7, SEND_FAIL.into())]
        );
        assert_eq!(
            tg.replies.lock().unwrap().as_slice(),
            &[(42, "modem error: error".into(), 7)]
        );
        assert!(modem.deleted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn send_and_ack_err_posts_error_without_delete() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem {
            fail: true,
            ..FakeModem::default()
        };
        let err = send_and_ack(&db, "+98912", "hi", 42, None, &modem, &tg, true)
            .await
            .unwrap_err();
        assert!(matches!(err, ActionError::ModemFailed(_)));
        assert!(tg.reactions.lock().unwrap().is_empty());
        assert!(modem.deleted.lock().unwrap().is_empty());
        assert_eq!(
            tg.posts.lock().unwrap().as_slice(),
            &[(42, "modem error: error".into())]
        );
    }

    #[tokio::test]
    async fn send_and_ack_pending_react_fail_skips_modem() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg {
            fail: true,
            ..FakeTg::new()
        };
        let modem = FakeModem::default();
        let err = send_and_ack(&db, "+98912", "hi", 42, Some(7), &modem, &tg, true)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ActionError::TelegramFailed { sent: false, .. }
        ));
        assert!(modem.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn watch_inbox_processes_existing_then_stops_on_cancel() {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let tg = Arc::new(FakeTg::new());
        let modem = Arc::new(FakeModem {
            listed: Mutex::new(vec![IncomingSms {
                path: "/sms/1".into(),
                e164: "+989120000001".into(),
                text: "hi".into(),
                inbound: true,
                timestamp: chrono::Utc::now().to_rfc3339(),
            }]),
            ..FakeModem::default()
        });
        let cancel = CancellationToken::new();
        let task = tokio::spawn(watch_inbox(
            modem.clone(),
            db,
            "IR".into(),
            tg.clone(),
            true,
            cancel.clone(),
            Duration::from_secs(30),
        ));
        tokio::time::sleep(Duration::from_millis(40)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("watch_inbox did not stop")
            .expect("watch_inbox join");
        assert_eq!(
            tg.posts.lock().unwrap().as_slice(),
            &[(1, "+989120000001\nhi".into())]
        );
        assert_eq!(
            modem.deleted.lock().unwrap().as_slice(),
            &["/sms/1".into()] as &[String]
        );
    }

    #[tokio::test]
    async fn send_failure_posts_error_keeps_going() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: Some(id),
            default_e164: Some("+989121234567".into()),
            title: "Ali (4567)".into(),
            ignored: false,
        })
        .unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem {
            fail: true,
            ..FakeModem::default()
        };
        handle_owner_text(&db, "IR", 42, "hello", Some(7), &modem, &tg, true)
            .await
            .unwrap();
        assert_eq!(
            tg.reactions.lock().unwrap().as_slice(),
            &[(7, SEND_PENDING.into()), (7, SEND_FAIL.into())]
        );
        let replies = tg.replies.lock().unwrap();
        assert!(replies[0].1.contains("error"));
        assert_eq!(replies[0].2, 7);
    }

    #[tokio::test]
    async fn owner_text_two_numbers_asks_and_saves_pending() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989188086139".into(), "+989025438263".into()])
            .unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: Some(id),
            default_e164: None,
            title: "Ali".into(),
            ignored: false,
        })
        .unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        let out = handle_owner_text(&db, "IR", 42, "hello", Some(7), &modem, &tg, true)
            .await
            .unwrap();
        assert_eq!(
            out,
            OwnerTextOutcome::NeedNumber(vec!["+989188086139".into(), "+989025438263".into()])
        );
        assert!(modem.sent.lock().unwrap().is_empty());
        assert!(tg.posts.lock().unwrap().is_empty());
        assert_eq!(
            db.take_pending_outbound(42).unwrap(),
            Some(("hello".into(), Some(7)))
        );
    }

    #[tokio::test]
    async fn owner_text_send_ok_deletes_path() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: Some(id),
            default_e164: Some("+989121234567".into()),
            title: "Ali (4567)".into(),
            ignored: false,
        })
        .unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        handle_owner_text(&db, "IR", 42, "hello", None, &modem, &tg, true)
            .await
            .unwrap();
        assert_eq!(
            modem.deleted.lock().unwrap().as_slice(),
            &["/fake/sms/1".into()] as &[String]
        );
    }

    #[tokio::test]
    async fn owner_text_send_err_does_not_delete() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: Some(id),
            default_e164: Some("+989121234567".into()),
            title: "Ali (4567)".into(),
            ignored: false,
        })
        .unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem {
            fail: true,
            ..FakeModem::default()
        };
        handle_owner_text(&db, "IR", 42, "hello", None, &modem, &tg, true)
            .await
            .unwrap();
        assert!(modem.deleted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn owner_text_disabled_does_not_delete() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: Some(id),
            default_e164: Some("+989121234567".into()),
            title: "Ali (4567)".into(),
            ignored: false,
        })
        .unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        handle_owner_text(&db, "IR", 42, "hello", None, &modem, &tg, false)
            .await
            .unwrap();
        assert!(modem.sent.lock().unwrap().len() == 1);
        assert!(modem.deleted.lock().unwrap().is_empty());
    }

    fn sample_inbound(path: &str) -> IncomingSms {
        IncomingSms {
            path: path.into(),
            e164: "09121234567".into(),
            text: "hi".into(),
            inbound: true,
            timestamp: chrono::Utc::now().to_rfc3339(),
        }
    }

    #[tokio::test]
    async fn maybe_delete_noop_when_disabled_or_empty() {
        let m = FakeModem::default();
        maybe_delete(false, &m, "/sms/1").await;
        maybe_delete(true, &m, "").await;
        assert!(m.deleted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn maybe_delete_records_path_and_swallows_error() {
        let m = FakeModem::default();
        maybe_delete(true, &m, "/sms/1").await;
        assert_eq!(
            m.deleted.lock().unwrap().as_slice(),
            &["/sms/1".into()] as &[String]
        );
        let m = FakeModem {
            delete_fail: true,
            ..FakeModem::default()
        };
        maybe_delete(true, &m, "/sms/1").await; // must not panic
    }

    #[tokio::test]
    async fn incoming_ok_deletes() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        handle_incoming_then_delete(&db, "IR", sample_inbound("/sms/1"), &tg, &modem, true)
            .await
            .unwrap();
        assert_eq!(
            modem.deleted.lock().unwrap().as_slice(),
            &["/sms/1".into()] as &[String]
        );
    }

    #[tokio::test]
    async fn incoming_seen_still_deletes() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        let sms = sample_inbound("/sms/1");
        handle_incoming_then_delete(&db, "IR", sms.clone(), &tg, &modem, true)
            .await
            .unwrap();
        handle_incoming_then_delete(&db, "IR", sms, &tg, &modem, true)
            .await
            .unwrap();
        assert_eq!(tg.posts.lock().unwrap().len(), 1);
        assert_eq!(
            modem.deleted.lock().unwrap().as_slice(),
            &["/sms/1".into(), "/sms/1".into()] as &[String]
        );
    }

    #[tokio::test]
    async fn incoming_stale_skip_deletes() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        handle_incoming_then_delete(
            &db,
            "IR",
            IncomingSms {
                path: "/sms/old".into(),
                e164: "+989120000002".into(),
                text: "old".into(),
                inbound: true,
                timestamp: "2024-12-21T15:12:23+03:30".into(),
            },
            &tg,
            &modem,
            true,
        )
        .await
        .unwrap();
        assert!(tg.posts.lock().unwrap().is_empty());
        assert_eq!(
            modem.deleted.lock().unwrap().as_slice(),
            &["/sms/old".into()] as &[String]
        );
    }

    #[tokio::test]
    async fn incoming_outbound_echo_deletes() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        handle_incoming_then_delete(
            &db,
            "IR",
            IncomingSms {
                path: "/sms/out".into(),
                e164: "+98912".into(),
                text: "x".into(),
                inbound: false,
                timestamp: "".into(),
            },
            &tg,
            &modem,
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            modem.deleted.lock().unwrap().as_slice(),
            &["/sms/out".into()] as &[String]
        );
    }

    #[tokio::test]
    async fn incoming_telegram_fail_does_not_delete() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: Some(id),
            default_e164: Some("+989121234567".into()),
            title: "Ali (4567)".into(),
            ignored: false,
        })
        .unwrap();
        let tg = FakeTg {
            fail: true,
            ..FakeTg::new()
        };
        let modem = FakeModem::default();
        assert!(handle_incoming_then_delete(
            &db,
            "IR",
            sample_inbound("/sms/1"),
            &tg,
            &modem,
            true
        )
        .await
        .is_err());
        assert!(modem.deleted.lock().unwrap().is_empty());
        assert!(!db.seen_sms_path("/sms/1").unwrap());
    }

    #[tokio::test]
    async fn incoming_empty_text_does_not_post_or_delete() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: Some(id),
            default_e164: Some("+989121234567".into()),
            title: "Ali (4567)".into(),
            ignored: false,
        })
        .unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        let sms = IncomingSms {
            path: "/sms/empty".into(),
            e164: "09121234567".into(),
            text: String::new(),
            inbound: true,
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        handle_incoming_then_delete(&db, "IR", sms, &tg, &modem, true)
            .await
            .unwrap();
        assert!(tg.posts.lock().unwrap().is_empty());
        assert!(modem.deleted.lock().unwrap().is_empty());
        assert!(!db.seen_sms_path("/sms/empty").unwrap());
    }

    #[tokio::test]
    async fn incoming_disabled_does_not_delete() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        handle_incoming_then_delete(&db, "IR", sample_inbound("/sms/1"), &tg, &modem, false)
            .await
            .unwrap();
        assert!(modem.deleted.lock().unwrap().is_empty());
    }

    fn sweep_now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-08-19T12:00:00+00:00")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn sweep_sms(inbound: bool, ts: &str) -> IncomingSms {
        IncomingSms {
            path: "/sms/x".into(),
            e164: "+98912".into(),
            text: "hi".into(),
            inbound,
            timestamp: ts.into(),
        }
    }

    #[test]
    fn sweep_action_table() {
        let now = sweep_now();
        let max = Duration::from_secs(30 * 86400);
        let win = Duration::from_secs(24 * 3600);
        let cases: &[(&str, IncomingSms, bool, SweepAction)] = &[
            (
                "unmarked inbound 1h",
                sweep_sms(true, "2026-08-19T11:00:00+00:00"),
                false,
                SweepAction::Keep,
            ),
            (
                "unmarked inbound no ts",
                sweep_sms(true, ""),
                false,
                SweepAction::Keep,
            ),
            (
                "unmarked inbound 2d",
                sweep_sms(true, "2026-08-17T12:00:00+00:00"),
                false,
                SweepAction::Keep,
            ),
            (
                "unmarked inbound 40d",
                sweep_sms(true, "2026-07-10T12:00:00+00:00"),
                false,
                SweepAction::Delete,
            ),
            (
                "seen inbound no ts",
                sweep_sms(true, ""),
                true,
                SweepAction::Delete,
            ),
            (
                "outbound no ts",
                sweep_sms(false, ""),
                false,
                SweepAction::Delete,
            ),
            (
                "outbound 40d",
                sweep_sms(false, "2026-07-10T12:00:00+00:00"),
                false,
                SweepAction::Delete,
            ),
            (
                "seen inbound 1h",
                sweep_sms(true, "2026-08-19T11:00:00+00:00"),
                true,
                SweepAction::Keep,
            ),
            (
                "outbound 1h",
                sweep_sms(false, "2026-08-19T11:00:00+00:00"),
                false,
                SweepAction::Keep,
            ),
        ];
        for (name, sms, seen, want) in cases {
            assert_eq!(sweep_action(sms, *seen, now, max, win), *want, "{name}");
        }
    }

    #[test]
    fn sweep_action_max_age_zero_keeps_fresh_unmarked() {
        let now = sweep_now();
        let max = Duration::from_secs(0);
        let win = Duration::from_secs(24 * 3600);
        assert_eq!(
            sweep_action(
                &sweep_sms(true, "2026-08-19T11:00:00+00:00"),
                false,
                now,
                max,
                win
            ),
            SweepAction::Keep
        );
        assert_eq!(
            sweep_action(
                &sweep_sms(false, "2026-08-19T11:00:00+00:00"),
                false,
                now,
                max,
                win
            ),
            SweepAction::Delete
        );
    }
}
