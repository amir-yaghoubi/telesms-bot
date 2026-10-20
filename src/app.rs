use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::actions::ActionError;
use crate::db::{Db, StoredClassification, Topic};
use crate::events::{EventBus, MessageEvent};
use crate::google::{ContactsSync, GoogleError};
use crate::modem::{IncomingSms, ModemError, ModemInfo, SmsInbox, SmsModem};
use crate::modem_mm::MmModem;
use crate::normalize::normalize_e164;
use crate::otp::extract_code;
use crate::route::{
    plan_outbound, route_for_send, route_inbound, InboundDest, OutboundPlan, GENERAL_THREAD,
};
use crate::sorting::config::SortingConfig;
use crate::sorting::decide::decide_sort;
use crate::sorting::render::render_inbound;
use crate::sorting::{Sorting, TEACH_CALLBACK};

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

/// Tracks present/absent transitions. Starts assumed present so the first
/// absence is an offline edge and a later return is a back edge. Use
/// [`Presence::peek`] + [`Presence::commit`] when the side effect can fail, so
/// the edge is only consumed once delivery succeeds; use
/// [`Presence::observe`] to commit and get the edge in one step.
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

    /// Returns the edge that would occur if `present` is committed, without
    /// committing it. Pair with [`Presence::commit`] after the side effect
    /// succeeds.
    pub fn peek(&mut self, present: bool) -> Option<PresenceEvent> {
        if self.last == present {
            return None;
        }
        Some(if present {
            PresenceEvent::Back
        } else {
            PresenceEvent::Offline
        })
    }

    pub fn commit(&mut self, present: bool) {
        self.last = present;
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
        if let Some(event) = presence.peek(present) {
            let text = match event {
                PresenceEvent::Offline => "modem offline",
                PresenceEvent::Back => "modem back",
            };
            match tg.post(GENERAL_THREAD, text.to_string()).await {
                Ok(()) => presence.commit(present),
                Err(err) => tracing::error!(error = %err, "failed to post modem presence"),
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    Ok,
    Degraded,
    Auth,
}

pub(crate) async fn sync_with_retries(
    syncer: &dyn ContactsSync,
    db: &Db,
    region: &str,
    retry_waits: &[Duration],
) -> SyncOutcome {
    let mut attempt = 0;
    loop {
        match syncer.sync_all(db, region).await {
            Ok(n) => {
                tracing::info!(contacts = n, "google contacts synced");
                return SyncOutcome::Ok;
            }
            Err(GoogleError::Auth(err)) => {
                tracing::error!(error = %err, "google contacts token rejected");
                return SyncOutcome::Auth;
            }
            Err(GoogleError::Transient(err)) => {
                if attempt >= retry_waits.len() {
                    tracing::warn!(error = %err, "google contacts sync failed after retries");
                    return SyncOutcome::Degraded;
                }
                let wait = retry_waits[attempt];
                attempt += 1;
                tracing::warn!(
                    error = %err,
                    wait_secs = wait.as_secs(),
                    "google contacts sync failed, retrying"
                );
                tokio::time::sleep(wait).await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "google contacts sync failed");
                return SyncOutcome::Degraded;
            }
        }
    }
}

async fn sync_once(
    syncer: &dyn ContactsSync,
    db: &Db,
    region: &str,
    tg: &dyn TelegramSink,
    retry_waits: &[Duration],
    presence: &mut Presence,
) {
    let outcome = sync_with_retries(syncer, db, region, retry_waits).await;
    match outcome {
        SyncOutcome::Ok => {
            db.set_contacts_available(true);
        }
        SyncOutcome::Degraded => {
            tracing::warn!("google contacts degraded; keeping last known contacts");
        }
        SyncOutcome::Auth => {
            db.set_contacts_available(false);
        }
    }
    let available = match outcome {
        SyncOutcome::Ok => Some(true),
        SyncOutcome::Auth => Some(false),
        SyncOutcome::Degraded => None,
    };
    if let Some(available) = available {
        if let Some(event) = presence.peek(available) {
            let text = match event {
                PresenceEvent::Offline => "⚠️ Google contacts token dead — run cargo run -- auth",
                PresenceEvent::Back => "google contacts back",
            };
            match tg.post(GENERAL_THREAD, text.to_string()).await {
                Ok(()) => presence.commit(available),
                Err(err) => tracing::error!(error = %err, "failed to post contacts health"),
            }
        }
    }
}

pub async fn watch_contacts(
    syncer: Arc<dyn ContactsSync>,
    db: Arc<Db>,
    region: String,
    tg: Arc<dyn TelegramSink>,
    interval: Duration,
    retry_waits: Vec<Duration>,
    cancel: CancellationToken,
) {
    let mut presence = Presence::new();
    sync_once(
        syncer.as_ref(),
        &db,
        &region,
        tg.as_ref(),
        &retry_waits,
        &mut presence,
    )
    .await;
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = ticker.tick() => {
                sync_once(
                    syncer.as_ref(),
                    &db,
                    &region,
                    tg.as_ref(),
                    &retry_waits,
                    &mut presence,
                )
                .await;
            }
        }
    }
}

/// The dependencies a send needs besides the message itself.
///
/// A send always touches the modem, the Telegram sink and the event bus, so
/// they travel together instead of as a growing argument list.
#[derive(Clone, Copy)]
pub struct SendContext<'a> {
    pub modem: &'a dyn SmsModem,
    pub tg: &'a dyn TelegramSink,
    /// Where the sent message is announced.
    pub bus: &'a EventBus,
    pub delete_enabled: bool,
}

/// Owned send dependencies, for callers that keep them around.
#[derive(Clone)]
pub struct SendDeps {
    pub modem: Arc<dyn SmsModem>,
    /// Where the sent message is announced.
    pub bus: EventBus,
}

/// State the daemon's transports share.
///
/// The HTTP API and the WebSocket server both work against the same modem,
/// database, Telegram sink and event bus.
#[derive(Clone)]
pub struct AppState {
    pub cfg: crate::config::Config,
    pub db: Arc<Db>,
    pub modem: Arc<dyn SmsModem>,
    pub info: Arc<dyn ModemInfo>,
    pub forward: Arc<dyn crate::modem::CallForward>,
    pub tg: Arc<dyn TelegramSink>,
    /// Where the application layer announces recorded messages.
    pub bus: EventBus,
    /// The sort config and Classifier, for the OTP learn endpoint.
    pub sorting: crate::sorting::Sorting,
}

/// Dependencies the inbox watcher needs, kept together so the watcher and the
/// functions it drives stay readable.
pub struct InboxWatch {
    pub db: Arc<Db>,
    pub region: String,
    pub tg: Arc<dyn TelegramSink>,
    pub delete_enabled: bool,
    /// Where the watcher announces the messages it handles.
    pub bus: EventBus,
    pub sorting: Sorting,
}

pub async fn watch_inbox<I>(
    inbox: Arc<I>,
    watch: InboxWatch,
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
                    watch.db.as_ref(),
                    &watch.region,
                    watch.tg.as_ref(),
                    watch.delete_enabled,
                    &cancel,
                    &watch.bus,
                    &watch.sorting,
                )
                .await;
                let mut stream = std::pin::pin!(stream);
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(retry) => {
                            drain_listed_sms(
                                inbox.as_ref(),
                                watch.db.as_ref(),
                                &watch.region,
                                watch.tg.as_ref(),
                                watch.delete_enabled,
                                &cancel,
                                &watch.bus,
                                &watch.sorting,
                            )
                            .await;
                        }
                        sms = stream.next() => {
                            let Some(sms) = sms else {
                                tracing::warn!("modem added signal stream ended");
                                break;
                            };
                            if let Err(err) = handle_incoming_then_delete(
                                watch.db.as_ref(),
                                &watch.region,
                                sms,
                                watch.tg.as_ref(),
                                inbox.as_ref(),
                                watch.delete_enabled,
                                &watch.bus,
                                &watch.sorting,
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

/// A post with HTML, an optional silent flag and one row of inline buttons.
#[derive(Clone, Debug, PartialEq)]
pub struct RichPost {
    pub html: String,
    pub silent: bool,
    /// (label, callback data), laid out as one row.
    pub buttons: Vec<(String, String)>,
}

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
    /// Posts HTML and returns the Telegram message id.
    async fn post_rich(&self, _thread_id: i32, _post: RichPost) -> Result<i32, AppError> {
        Err(AppError::Telegram("post_rich not supported".into()))
    }
    /// Replaces a message's text with HTML and drops its inline keyboard.
    async fn edit_html(&self, _message_id: i32, _html: String) -> Result<(), AppError> {
        Err(AppError::Telegram("edit_html not supported".into()))
    }
    /// Sends a force-reply prompt in the thread, replying to `reply_to` when given.
    async fn prompt_reply(
        &self,
        _thread_id: i32,
        _text: String,
        _reply_to: Option<i32>,
    ) -> Result<(), AppError> {
        Err(AppError::Telegram("prompt_reply not supported".into()))
    }
}

pub struct FakeTg {
    pub posts: Mutex<Vec<(i32, String)>>,
    pub replies: Mutex<Vec<(i32, String, i32)>>,
    pub reactions: Mutex<Vec<(i32, String)>>,
    pub rich_posts: Mutex<Vec<(i32, RichPost)>>,
    pub edits: Mutex<Vec<(i32, String)>>,
    pub prompts: Mutex<Vec<(i32, String, Option<i32>)>>,
    pub next_thread: AtomicI32,
    pub next_message: AtomicI32,
    pub fail: AtomicBool,
}

impl FakeTg {
    pub fn new() -> Self {
        Self {
            posts: Mutex::new(Vec::new()),
            replies: Mutex::new(Vec::new()),
            reactions: Mutex::new(Vec::new()),
            rich_posts: Mutex::new(Vec::new()),
            edits: Mutex::new(Vec::new()),
            prompts: Mutex::new(Vec::new()),
            next_thread: AtomicI32::new(100),
            next_message: AtomicI32::new(500),
            fail: AtomicBool::new(false),
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
        if self.fail.load(Ordering::SeqCst) {
            return Err(AppError::Telegram("fail".into()));
        }
        self.posts
            .lock()
            .expect("fake tg posts lock")
            .push((thread_id, text));
        Ok(())
    }

    async fn reply(&self, thread_id: i32, text: String, reply_to: i32) -> Result<(), AppError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(AppError::Telegram("fail".into()));
        }
        self.replies
            .lock()
            .expect("fake tg replies lock")
            .push((thread_id, text, reply_to));
        Ok(())
    }

    async fn react(&self, message_id: i32, emoji: &str) -> Result<(), AppError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(AppError::Telegram("fail".into()));
        }
        self.reactions
            .lock()
            .expect("fake tg reactions lock")
            .push((message_id, emoji.to_string()));
        Ok(())
    }

    async fn create_topic(&self, _title: String) -> Result<i32, AppError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(AppError::Telegram("fail".into()));
        }
        Ok(self.next_thread.fetch_add(1, Ordering::SeqCst))
    }

    async fn post_rich(&self, thread_id: i32, post: RichPost) -> Result<i32, AppError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(AppError::Telegram("fail".into()));
        }
        self.rich_posts
            .lock()
            .expect("fake tg rich posts lock")
            .push((thread_id, post));
        Ok(self.next_message.fetch_add(1, Ordering::SeqCst))
    }

    async fn edit_html(&self, message_id: i32, html: String) -> Result<(), AppError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(AppError::Telegram("fail".into()));
        }
        self.edits
            .lock()
            .expect("fake tg edits lock")
            .push((message_id, html));
        Ok(())
    }

    async fn prompt_reply(
        &self,
        thread_id: i32,
        text: String,
        reply_to: Option<i32>,
    ) -> Result<(), AppError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(AppError::Telegram("fail".into()));
        }
        self.prompts
            .lock()
            .expect("fake tg prompts lock")
            .push((thread_id, text, reply_to));
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn drain_listed_sms<I>(
    inbox: &I,
    db: &Db,
    region: &str,
    tg: &dyn TelegramSink,
    delete_enabled: bool,
    cancel: &CancellationToken,
    bus: &EventBus,
    sorting: &Sorting,
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
                if let Err(err) = handle_incoming_then_delete(
                    db,
                    region,
                    sms,
                    tg,
                    inbox,
                    delete_enabled,
                    bus,
                    sorting,
                )
                .await
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

#[allow(clippy::too_many_arguments)]
pub async fn handle_incoming_then_delete(
    db: &Db,
    region: &str,
    sms: IncomingSms,
    tg: &dyn TelegramSink,
    modem: &dyn SmsModem,
    delete_enabled: bool,
    bus: &EventBus,
    sorting: &Sorting,
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
    match handle_incoming(db, region, sms, tg, bus, sorting).await {
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
    bus: &EventBus,
    sorting: &Sorting,
) -> Result<(), AppError> {
    if !sms.inbound {
        return Ok(());
    }
    let id_e164 = normalize_e164(&sms.e164, region).unwrap_or_else(|_| sms.e164.clone());
    if db.seen_sms(&sms.path, &id_e164, &sms.text, &sms.timestamp)? {
        return Ok(());
    }
    let patterns = sorting.patterns(db)?;
    let otp_code = extract_code(&patterns, &sms.text).map(|(_, code)| code);
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
        let understood = Understood {
            otp_code: otp_code.as_deref(),
            classified: None,
        };
        announce_inbound(db, bus, &sms, &id_e164, thread_id, None, &understood)?;
        return Ok(());
    }

    let classified = classify_inbound(sorting, &id_e164, &sms.text).await;
    let understood = Understood {
        otp_code: otp_code.as_deref(),
        classified: classified.as_ref(),
    };
    match deliver_incoming(db, region, &sms, tg, sorting, &understood).await {
        Ok(Delivered::Normalized {
            e164,
            thread_id,
            tg_msg,
        }) => {
            db.mark_incoming(&e164)?;
            announce_inbound(db, bus, &sms, &e164, thread_id, Some(tg_msg), &understood)?;
            Ok(())
        }
        Ok(Delivered::Raw { thread_id, tg_msg }) => {
            announce_inbound(
                db,
                bus,
                &sms,
                &id_e164,
                thread_id,
                Some(tg_msg),
                &understood,
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

/// What the gateway understood about an inbound message before posting it.
struct Understood<'a> {
    otp_code: Option<&'a str>,
    classified: Option<&'a StoredClassification>,
}

/// The Classification of an inbound message. A failure is not an error: the
/// message is delivered unclassified and classified again later.
async fn classify_inbound(
    sorting: &Sorting,
    e164: &str,
    body: &str,
) -> Option<StoredClassification> {
    let classifier = sorting.classifier.as_ref()?;
    match classifier.classify(e164, body).await {
        Ok(classified) => Some(classified),
        Err(err) => {
            tracing::warn!(e164, error = %err, "classify inbound sms failed");
            None
        }
    }
}

/// Record an inbound SMS and announce it to realtime subscribers.
fn announce_inbound(
    db: &Db,
    bus: &EventBus,
    sms: &IncomingSms,
    e164: &str,
    thread_id: i32,
    tg_msg: Option<i32>,
    understood: &Understood<'_>,
) -> Result<(), AppError> {
    let row = db.record_inbound(
        &sms.path,
        e164,
        &sms.text,
        tg_msg,
        &sms.timestamp,
        thread_id,
        understood.otp_code,
        understood.classified,
    )?;
    bus.publish(MessageEvent::from(row));
    Ok(())
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

/// Inbound messages whose text is still empty after this long are
/// incomplete multipart SMS whose remaining parts will never arrive;
/// they block SIM storage slots forever if not removed.
pub const STUCK_RECEIVING_MAX: Duration = Duration::from_secs(6 * 3600);

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
        if sms.text.is_empty() {
            return match age {
                None => SweepAction::Keep,
                Some(age) if age > STUCK_RECEIVING_MAX => SweepAction::Delete,
                Some(_) => SweepAction::Keep,
            };
        }
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
    Normalized {
        e164: String,
        thread_id: i32,
        tg_msg: i32,
    },
    Raw {
        thread_id: i32,
        tg_msg: i32,
    },
}

/// Posts the message and returns where it landed. Nothing is recorded here:
/// the caller records only after Telegram accepted the post, so a failed post
/// leaves the message unseen and it is retried.
async fn deliver_incoming(
    db: &Db,
    region: &str,
    sms: &IncomingSms,
    tg: &dyn TelegramSink,
    sorting: &Sorting,
    understood: &Understood<'_>,
) -> Result<Delivered, AppError> {
    let e164 = match normalize_e164(&sms.e164, region) {
        Ok(e164) => e164,
        Err(_) => {
            let (thread_id, tg_msg) =
                post_to_general(db, tg, sorting, understood, sms, &sms.e164).await?;
            return Ok(Delivered::Raw { thread_id, tg_msg });
        }
    };

    let (thread_id, tg_msg) = match route_inbound(db, &e164)? {
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
                sort: None,
            })?;
            let tg_msg = post_inbound(tg, sorting, understood, thread_id, None, false, sms).await?;
            (thread_id, tg_msg)
        }
        InboundDest::ExistingTopic {
            thread_id,
            note_switch_to,
        } => {
            if let Some(switched) = note_switch_to {
                tg.post(thread_id, format!("now using {switched}")).await?;
            }
            let tg_msg = post_inbound(tg, sorting, understood, thread_id, None, false, sms).await?;
            (thread_id, tg_msg)
        }
        InboundDest::General { e164: dest_e164 } => {
            post_to_general(db, tg, sorting, understood, sms, &dest_e164).await?
        }
    };

    Ok(Delivered::Normalized {
        e164,
        thread_id,
        tg_msg,
    })
}

/// Posts a message that would land in General, in its Sorted Thread when it has a Sort.
async fn post_to_general(
    db: &Db,
    tg: &dyn TelegramSink,
    sorting: &Sorting,
    understood: &Understood<'_>,
    sms: &IncomingSms,
    number: &str,
) -> Result<(i32, i32), AppError> {
    let sort = decide_sort(
        understood.classified.map(|c| &c.public),
        understood.otp_code,
        &sorting.config,
    );
    let (thread_id, silent) = match sort {
        Some(sort) => {
            let thread_id = sorted_thread(db, tg, &sorting.config, &sort).await?;
            (thread_id, sorting.config.sorts[&sort].muted)
        }
        None => (GENERAL_THREAD, false),
    };
    let tg_msg = post_inbound(
        tg,
        sorting,
        understood,
        thread_id,
        Some(number),
        silent,
        sms,
    )
    .await?;
    Ok((thread_id, tg_msg))
}

/// The thread for a Sort, created on first use.
async fn sorted_thread(
    db: &Db,
    tg: &dyn TelegramSink,
    cfg: &SortingConfig,
    sort: &str,
) -> Result<i32, AppError> {
    if let Some(thread_id) = db.sorted_topic(sort)? {
        return Ok(thread_id);
    }
    let title = cfg.sorts[sort].title.clone();
    let thread_id = tg.create_topic(title.clone()).await?;
    db.upsert_sorted_topic(sort, thread_id, &title)?;
    Ok(thread_id)
}

async fn post_inbound(
    tg: &dyn TelegramSink,
    sorting: &Sorting,
    understood: &Understood<'_>,
    thread_id: i32,
    number_line: Option<&str>,
    silent: bool,
    sms: &IncomingSms,
) -> Result<i32, AppError> {
    let post = render_inbound(
        understood.classified.map(|c| &c.public),
        understood.otp_code,
        number_line,
        &sms.text,
        &sorting.config,
    );
    let buttons = if post.teach_button {
        vec![("Teach code".to_string(), TEACH_CALLBACK.to_string())]
    } else {
        Vec::new()
    };
    tg.post_rich(
        thread_id,
        RichPost {
            html: post.html,
            silent,
            buttons,
        },
    )
    .await
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
    ctx: &SendContext<'_>,
) -> Result<(), ActionError> {
    let SendContext {
        modem,
        tg,
        bus,
        delete_enabled,
    } = *ctx;
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
            let row = db.record_outbound(e164, text, "ok", thread_id)?;
            bus.publish(MessageEvent::from(row));
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
            let row = db.record_outbound(e164, text, &err_s, thread_id)?;
            bus.publish(MessageEvent::from(row));
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
    ctx: &SendContext<'_>,
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
            match send_and_ack(db, &e164, text, thread_id, reply_to, ctx).await {
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
    use crate::db::{Classification, Db};
    use crate::modem::FakeModem;
    use crate::sorting::classifier::{ClassifyError, FakeClassifier};
    use crate::sorting::guard::GuardedClassifier;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    /// A bus to announce through; tests that do not care ignore it.
    fn bus() -> EventBus {
        EventBus::new(16)
    }

    /// A send context for tests.
    fn send_context<'a>(
        modem: &'a dyn SmsModem,
        tg: &'a dyn TelegramSink,
        bus: &'a EventBus,
    ) -> SendContext<'a> {
        SendContext {
            modem,
            tg,
            bus,
            delete_enabled: true,
        }
    }

    /// A send context that leaves the sent SMS on the modem.
    fn send_context_without_delete<'a>(
        modem: &'a dyn SmsModem,
        tg: &'a dyn TelegramSink,
        bus: &'a EventBus,
    ) -> SendContext<'a> {
        SendContext {
            modem,
            tg,
            bus,
            delete_enabled: false,
        }
    }

    #[tokio::test]
    async fn an_inbound_sms_is_announced() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let bus = bus();
        let mut events = bus.subscribe();

        handle_incoming(
            &db,
            "IR",
            IncomingSms {
                path: "/sms/1".into(),
                e164: "+989120000001".into(),
                text: "hi".into(),
                inbound: true,
                timestamp: chrono::Utc::now().to_rfc3339(),
            },
            &tg,
            &bus,
            &Sorting::disabled(),
        )
        .await
        .unwrap();

        let event = events.try_recv().expect("announced");
        assert_eq!(event.direction, "in");
        assert_eq!(event.e164, "+989120000001");
        assert_eq!(event.body, "hi");
        assert_eq!(event.status, "ok");
    }

    #[tokio::test]
    async fn an_outbound_sms_is_announced_with_its_status() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        let bus = bus();
        let mut events = bus.subscribe();

        send_and_ack(
            &db,
            "+989120000001",
            "hi",
            1,
            None,
            &send_context(&modem, &tg, &bus),
        )
        .await
        .unwrap();

        let event = events.try_recv().expect("announced");
        assert_eq!(event.direction, "out");
        assert_eq!(event.e164, "+989120000001");
        assert_eq!(event.body, "hi");
        assert_eq!(event.status, "ok");
    }

    #[tokio::test]
    async fn a_failed_outbound_sms_is_announced_as_failed() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem {
            fail: true,
            ..FakeModem::default()
        };
        let bus = bus();
        let mut events = bus.subscribe();

        let err = send_and_ack(
            &db,
            "+989120000001",
            "hi",
            1,
            None,
            &send_context(&modem, &tg, &bus),
        )
        .await;
        assert!(err.is_err());

        let event = events.try_recv().expect("announced");
        assert_eq!(event.direction, "out");
        assert_eq!(event.status, "failed");
    }

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
            &bus(),
            &Sorting::disabled(),
        )
        .await
        .unwrap();
        let posts = tg.rich_posts.lock().unwrap();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].1.html, "hi");
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
        handle_incoming(&db, "IR", sms.clone(), &tg, &bus(), &Sorting::disabled())
            .await
            .unwrap();
        handle_incoming(&db, "IR", sms, &tg, &bus(), &Sorting::disabled())
            .await
            .unwrap();
        assert_eq!(tg.rich_posts.lock().unwrap().len(), 1);
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
        handle_incoming(&db, "IR", a, &tg, &bus(), &Sorting::disabled())
            .await
            .unwrap();
        handle_incoming(&db, "IR", b, &tg, &bus(), &Sorting::disabled())
            .await
            .unwrap();
        assert_eq!(tg.rich_posts.lock().unwrap().len(), 1);
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
            &bus(),
            &Sorting::disabled(),
        )
        .await
        .unwrap();
        assert!(tg.posts.lock().unwrap().is_empty());
        assert!(tg.rich_posts.lock().unwrap().is_empty());
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
            sort: None,
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
            &bus(),
            &Sorting::disabled(),
        )
        .await
        .unwrap();
        assert!(tg.posts.lock().unwrap().is_empty());
        assert!(tg.rich_posts.lock().unwrap().is_empty());
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
            sort: None,
        })
        .unwrap();
        let tg = FakeTg {
            fail: AtomicBool::new(true),
            ..FakeTg::new()
        };
        let sms = IncomingSms {
            path: "/sms/1".into(),
            e164: "09121234567".into(),
            text: "secret body".into(),
            inbound: true,
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        assert!(
            handle_incoming(&db, "IR", sms, &tg, &bus(), &Sorting::disabled())
                .await
                .is_err()
        );
        assert!(!db.seen_sms_path("/sms/1").unwrap());
    }

    #[tokio::test]
    async fn owner_text_in_general_does_not_send() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        handle_owner_text(
            &db,
            "IR",
            1,
            "hello",
            None,
            &send_context(&modem, &tg, &bus()),
        )
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
            sort: None,
        })
        .unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        handle_owner_text(
            &db,
            "IR",
            42,
            "hello",
            Some(7),
            &send_context(&modem, &tg, &bus()),
        )
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
    async fn watch_modem_reposts_alert_when_telegram_fails() {
        let tg = Arc::new(FakeTg {
            fail: AtomicBool::new(true),
            ..FakeTg::new()
        });
        let modem: Arc<dyn ModemInfo> = Arc::new(FakeModem::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(watch_modem(
            modem,
            tg.clone(),
            Duration::from_millis(15),
            cancel.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(40)).await;
        tg.fail.store(false, Ordering::SeqCst);
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

    #[tokio::test]
    async fn fake_tg_records_rich_posts_with_ids() {
        let tg = FakeTg::new();
        let first = RichPost {
            html: "<b>a</b>".into(),
            silent: true,
            buttons: vec![("Move".into(), "mv:1".into())],
        };
        let second = RichPost {
            html: "b".into(),
            silent: false,
            buttons: vec![],
        };
        assert_eq!(tg.post_rich(7, first.clone()).await.unwrap(), 500);
        assert_eq!(tg.post_rich(8, second.clone()).await.unwrap(), 501);
        assert_eq!(
            tg.rich_posts.lock().unwrap().as_slice(),
            &[(7, first), (8, second)]
        );
        tg.edit_html(500, "<i>x</i>".into()).await.unwrap();
        tg.prompt_reply(7, "name?".into(), Some(9)).await.unwrap();
        assert_eq!(
            tg.edits.lock().unwrap().as_slice(),
            &[(500, "<i>x</i>".into())]
        );
        assert_eq!(
            tg.prompts.lock().unwrap().as_slice(),
            &[(7, "name?".into(), Some(9))]
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
        send_and_ack(
            &db,
            "+98912",
            "hi",
            42,
            Some(7),
            &send_context(&modem, &tg, &bus()),
        )
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
        send_and_ack(
            &db,
            "+98912",
            "hi",
            42,
            None,
            &send_context(&modem, &tg, &bus()),
        )
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
        let err = send_and_ack(
            &db,
            "+98912",
            "hi",
            42,
            Some(7),
            &send_context(&modem, &tg, &bus()),
        )
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
        let err = send_and_ack(
            &db,
            "+98912",
            "hi",
            42,
            None,
            &send_context(&modem, &tg, &bus()),
        )
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
            fail: AtomicBool::new(true),
            ..FakeTg::new()
        };
        let modem = FakeModem::default();
        let err = send_and_ack(
            &db,
            "+98912",
            "hi",
            42,
            Some(7),
            &send_context(&modem, &tg, &bus()),
        )
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
        let watch = InboxWatch {
            db,
            region: "IR".into(),
            tg: tg.clone(),
            delete_enabled: true,
            bus: bus(),
            sorting: Sorting::disabled(),
        };
        let task = tokio::spawn(watch_inbox(
            modem.clone(),
            watch,
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
            tg.rich_posts.lock().unwrap().as_slice(),
            &[(
                1,
                RichPost {
                    html: "+989120000001\nhi".into(),
                    silent: false,
                    buttons: vec![],
                }
            )]
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
            sort: None,
        })
        .unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem {
            fail: true,
            ..FakeModem::default()
        };
        handle_owner_text(
            &db,
            "IR",
            42,
            "hello",
            Some(7),
            &send_context(&modem, &tg, &bus()),
        )
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
            sort: None,
        })
        .unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        let out = handle_owner_text(
            &db,
            "IR",
            42,
            "hello",
            Some(7),
            &send_context(&modem, &tg, &bus()),
        )
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
            sort: None,
        })
        .unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        handle_owner_text(
            &db,
            "IR",
            42,
            "hello",
            None,
            &send_context(&modem, &tg, &bus()),
        )
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
            sort: None,
        })
        .unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem {
            fail: true,
            ..FakeModem::default()
        };
        handle_owner_text(
            &db,
            "IR",
            42,
            "hello",
            None,
            &send_context(&modem, &tg, &bus()),
        )
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
            sort: None,
        })
        .unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        handle_owner_text(
            &db,
            "IR",
            42,
            "hello",
            None,
            &send_context_without_delete(&modem, &tg, &bus()),
        )
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
        handle_incoming_then_delete(
            &db,
            "IR",
            sample_inbound("/sms/1"),
            &tg,
            &modem,
            true,
            &bus(),
            &Sorting::disabled(),
        )
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
        handle_incoming_then_delete(
            &db,
            "IR",
            sms.clone(),
            &tg,
            &modem,
            true,
            &bus(),
            &Sorting::disabled(),
        )
        .await
        .unwrap();
        handle_incoming_then_delete(
            &db,
            "IR",
            sms,
            &tg,
            &modem,
            true,
            &bus(),
            &Sorting::disabled(),
        )
        .await
        .unwrap();
        assert_eq!(tg.rich_posts.lock().unwrap().len(), 1);
        assert_eq!(
            modem.deleted.lock().unwrap().as_slice(),
            &["/sms/1".into(), "/sms/1".into()] as &[String]
        );
    }

    #[tokio::test]
    async fn incoming_new_message_on_recycled_path_delivers_once_and_deletes() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        // Stale row holds the modem path from before a ModemManager restart.
        db.record_inbound(
            "/sms/1",
            "+989199999999",
            "old promo",
            None,
            "2026-08-20T22:45:37+03:30",
            42,
            None,
            None,
        )
        .unwrap();
        let tg = FakeTg::new();
        let modem = FakeModem::default();
        let sms = IncomingSms {
            path: "/sms/1".into(),
            e164: "+989121234567".into(),
            text: "fresh message".into(),
            inbound: true,
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        handle_incoming_then_delete(
            &db,
            "IR",
            sms.clone(),
            &tg,
            &modem,
            true,
            &bus(),
            &Sorting::disabled(),
        )
        .await
        .unwrap();
        assert_eq!(tg.rich_posts.lock().unwrap().len(), 1);
        assert_eq!(
            modem.deleted.lock().unwrap().as_slice(),
            &["/sms/1".into()] as &[String]
        );
        // A later sweep sees the same message again: deduped, no repost.
        handle_incoming_then_delete(
            &db,
            "IR",
            sms,
            &tg,
            &modem,
            true,
            &bus(),
            &Sorting::disabled(),
        )
        .await
        .unwrap();
        assert_eq!(tg.rich_posts.lock().unwrap().len(), 1);
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
            &bus(),
            &Sorting::disabled(),
        )
        .await
        .unwrap();
        assert!(tg.posts.lock().unwrap().is_empty());
        assert!(tg.rich_posts.lock().unwrap().is_empty());
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
            &bus(),
            &Sorting::disabled(),
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
            sort: None,
        })
        .unwrap();
        let tg = FakeTg {
            fail: AtomicBool::new(true),
            ..FakeTg::new()
        };
        let modem = FakeModem::default();
        assert!(handle_incoming_then_delete(
            &db,
            "IR",
            sample_inbound("/sms/1"),
            &tg,
            &modem,
            true,
            &bus(),
            &Sorting::disabled(),
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
            sort: None,
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
        handle_incoming_then_delete(
            &db,
            "IR",
            sms,
            &tg,
            &modem,
            true,
            &bus(),
            &Sorting::disabled(),
        )
        .await
        .unwrap();
        assert!(tg.posts.lock().unwrap().is_empty());
        assert!(tg.rich_posts.lock().unwrap().is_empty());
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
        handle_incoming_then_delete(
            &db,
            "IR",
            sample_inbound("/sms/1"),
            &tg,
            &modem,
            false,
            &bus(),
            &Sorting::disabled(),
        )
        .await
        .unwrap();
        assert!(modem.deleted.lock().unwrap().is_empty());
    }

    fn stored(category: &str, confidence: f64, tags: &[&str]) -> StoredClassification {
        StoredClassification {
            public: Classification {
                category: category.into(),
                confidence,
                tags: tags.iter().map(|t| t.to_string()).collect(),
            },
            scores: serde_json::json!({}),
        }
    }

    fn sorting_with(answers: Vec<Result<StoredClassification, ClassifyError>>) -> Sorting {
        Sorting {
            config: Arc::new(SortingConfig::load(None).unwrap()),
            classifier: Some(Arc::new(FakeClassifier::new(answers))),
        }
    }

    fn unknown_sms(path: &str, text: &str) -> IncomingSms {
        IncomingSms {
            path: path.into(),
            e164: "+989981600237".into(),
            text: text.into(),
            inbound: true,
            timestamp: chrono::Utc::now().to_rfc3339(),
        }
    }

    fn recorded(db: &Db) -> Vec<crate::db::MessageRow> {
        db.messages_after("", 100).unwrap()
    }

    #[tokio::test]
    async fn promotional_from_unknown_number_goes_to_muted_sorted_thread() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let bus = bus();
        let mut events = bus.subscribe();
        let sorting = sorting_with(vec![Ok(stored("promotional", 0.9, &[]))]);

        handle_incoming(
            &db,
            "IR",
            unknown_sms("/sms/1", "off 50%"),
            &tg,
            &bus,
            &sorting,
        )
        .await
        .unwrap();

        let tid = db
            .sorted_topic("promotional")
            .unwrap()
            .expect("sorted topic");
        assert_eq!(
            db.get_topic_by_thread(tid).unwrap().unwrap().title,
            "تبلیغاتی"
        );
        let posts = tg.rich_posts.lock().unwrap();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].0, tid);
        assert!(posts[0].1.silent);
        assert_eq!(
            posts[0].1.html,
            "🟨 <b>تبلیغاتی</b>\n+989981600237\noff 50%"
        );
        assert!(tg.posts.lock().unwrap().is_empty());
        let announced = events.try_recv().expect("announced");
        assert_eq!(announced.thread_id, tid);
        assert_eq!(announced.classification.unwrap().category, "promotional");
        let rows = recorded(&db);
        assert_eq!(rows[0].thread_id, tid);
        assert_eq!(
            rows[0].classification.as_ref().unwrap().category,
            "promotional"
        );
    }

    #[tokio::test]
    async fn second_promotional_reuses_sorted_thread() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let sorting = sorting_with(vec![
            Ok(stored("promotional", 0.9, &[])),
            Ok(stored("promotional", 0.9, &[])),
        ]);

        for (path, text) in [("/sms/1", "first"), ("/sms/2", "second")] {
            handle_incoming(&db, "IR", unknown_sms(path, text), &tg, &bus(), &sorting)
                .await
                .unwrap();
        }

        let tid = db.sorted_topic("promotional").unwrap().unwrap();
        let posts = tg.rich_posts.lock().unwrap();
        assert_eq!(
            posts.iter().map(|p| p.0).collect::<Vec<_>>(),
            vec![tid, tid]
        );
        assert_eq!(tg.next_thread.load(Ordering::SeqCst), 101);
    }

    #[tokio::test]
    async fn otp_from_unknown_number_goes_to_loud_otp_thread() {
        let db = Db::open_in_memory().unwrap();
        db.add_otp_pattern(Some("bank"), r"رمز:\s+(\d{4,8})")
            .unwrap();
        let tg = FakeTg::new();
        let bus = bus();
        let mut events = bus.subscribe();
        let sorting = sorting_with(vec![Ok(stored("transactional", 0.9, &[]))]);

        handle_incoming(
            &db,
            "IR",
            unknown_sms("/sms/1", "رمز: 402332"),
            &tg,
            &bus,
            &sorting,
        )
        .await
        .unwrap();

        let tid = db.sorted_topic("otp").unwrap().expect("otp thread");
        let posts = tg.rich_posts.lock().unwrap();
        assert_eq!(posts[0].0, tid);
        assert!(!posts[0].1.silent);
        assert!(posts[0].1.html.contains("<code>402332</code>"));
        assert!(posts[0].1.buttons.is_empty());
        assert_eq!(recorded(&db)[0].otp_code.as_deref(), Some("402332"));
        assert_eq!(
            events.try_recv().expect("announced").otp_code.as_deref(),
            Some("402332")
        );
    }

    #[tokio::test]
    async fn scam_with_code_goes_to_scam_thread() {
        let db = Db::open_in_memory().unwrap();
        db.add_otp_pattern(Some("bank"), r"رمز:\s+(\d{4,8})")
            .unwrap();
        let tg = FakeTg::new();
        let sorting = sorting_with(vec![Ok(stored("promotional", 0.9, &["scam"]))]);

        handle_incoming(
            &db,
            "IR",
            unknown_sms("/sms/1", "رمز: 402332"),
            &tg,
            &bus(),
            &sorting,
        )
        .await
        .unwrap();

        let tid = db.sorted_topic("scam").unwrap().expect("scam thread");
        assert!(db.sorted_topic("otp").unwrap().is_none());
        assert_eq!(tg.rich_posts.lock().unwrap()[0].0, tid);
        assert_eq!(recorded(&db)[0].otp_code.as_deref(), Some("402332"));
    }

    #[tokio::test]
    async fn contact_message_stays_in_contact_thread() {
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
            sort: None,
        })
        .unwrap();
        let tg = FakeTg::new();
        let sorting = sorting_with(vec![Ok(stored("promotional", 0.9, &["scam"]))]);
        let mut sms = unknown_sms("/sms/1", "win big");
        sms.e164 = "09121234567".into();

        handle_incoming(&db, "IR", sms, &tg, &bus(), &sorting)
            .await
            .unwrap();

        let posts = tg.rich_posts.lock().unwrap();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].0, 42);
        assert!(!posts[0].1.silent);
        assert_eq!(
            posts[0].1.html,
            "🟨 <b>تبلیغاتی</b>  🟥 <b>کلاهبرداری/فیشینگ</b>\nwin big"
        );
        assert!(db.sorted_topic("promotional").unwrap().is_none());
        assert!(db.sorted_topic("scam").unwrap().is_none());
        assert_eq!(tg.next_thread.load(Ordering::SeqCst), 100);
        assert_eq!(recorded(&db)[0].thread_id, 42);
    }

    #[tokio::test(start_paused = true)]
    async fn classifier_failure_posts_unclassified_in_general() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let config = Arc::new(SortingConfig::load(None).unwrap());
        let fake = Arc::new(FakeClassifier::new(vec![
            Err(ClassifyError::Timeout),
            Err(ClassifyError::Timeout),
            Err(ClassifyError::Timeout),
        ]));
        let sorting = Sorting {
            classifier: Some(Arc::new(GuardedClassifier::new(
                fake.clone(),
                config.retry.clone(),
                config.breaker.clone(),
            ))),
            config,
        };
        let bus = bus();
        let mut events = bus.subscribe();

        handle_incoming(
            &db,
            "IR",
            unknown_sms("/sms/1", "hello"),
            &tg,
            &bus,
            &sorting,
        )
        .await
        .unwrap();

        assert_eq!(fake.calls.load(Ordering::SeqCst), 3);
        let posts = tg.rich_posts.lock().unwrap();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].0, GENERAL_THREAD);
        assert!(!posts[0].1.silent);
        assert_eq!(posts[0].1.html, "+989981600237\nhello");
        assert_eq!(events.try_recv().unwrap().thread_id, GENERAL_THREAD);
        assert!(recorded(&db)[0].classification.is_none());
        assert_eq!(
            db.pending_classification(None, &["pending"], None, 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn classifier_disabled_still_extracts_and_sorts_otp() {
        let db = Db::open_in_memory().unwrap();
        db.add_otp_pattern(Some("bank"), r"رمز:\s+(\d{4,8})")
            .unwrap();
        let tg = FakeTg::new();

        handle_incoming(
            &db,
            "IR",
            unknown_sms("/sms/1", "رمز: ۴۰۲۳۳۲"),
            &tg,
            &bus(),
            &Sorting::disabled(),
        )
        .await
        .unwrap();

        let tid = db.sorted_topic("otp").unwrap().expect("otp thread");
        assert_eq!(tg.rich_posts.lock().unwrap()[0].0, tid);
        assert_eq!(recorded(&db)[0].otp_code.as_deref(), Some("402332"));
    }

    #[tokio::test]
    async fn unlearned_otp_gets_teach_button() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();

        handle_incoming(
            &db,
            "IR",
            unknown_sms("/sms/1", "کد ورود: 5388"),
            &tg,
            &bus(),
            &Sorting::disabled(),
        )
        .await
        .unwrap();

        let posts = tg.rich_posts.lock().unwrap();
        assert_eq!(posts[0].0, GENERAL_THREAD);
        assert_eq!(
            posts[0].1.buttons,
            vec![("Teach code".to_string(), "otp:teach".to_string())]
        );
        let tg_msg = tg.next_message.load(Ordering::SeqCst) - 1;
        let row = db
            .inbound_by_tg_msg(tg_msg)
            .unwrap()
            .expect("recorded with tg_msg");
        assert_eq!(row.body, "کد ورود: 5388");
    }

    #[tokio::test]
    async fn raw_sender_is_general_bound_and_can_be_sorted() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let sorting = sorting_with(vec![Ok(stored("promotional", 0.9, &[]))]);
        let mut sms = unknown_sms("/sms/1", "sale");
        sms.e164 = "Snapp".into();

        handle_incoming(&db, "IR", sms, &tg, &bus(), &sorting)
            .await
            .unwrap();

        let tid = db
            .sorted_topic("promotional")
            .unwrap()
            .expect("sorted topic");
        let posts = tg.rich_posts.lock().unwrap();
        assert_eq!(posts[0].0, tid);
        assert_eq!(posts[0].1.html, "🟨 <b>تبلیغاتی</b>\nSnapp\nsale");
        assert_eq!(recorded(&db)[0].e164, "Snapp");
    }

    #[tokio::test]
    async fn telegram_failure_leaves_sms_unrecorded() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let sorting = sorting_with(vec![
            Ok(stored("promotional", 0.9, &[])),
            Ok(stored("promotional", 0.9, &[])),
        ]);
        let sms = unknown_sms("/sms/1", "off 50%");

        tg.fail.store(true, Ordering::SeqCst);
        assert!(
            handle_incoming(&db, "IR", sms.clone(), &tg, &bus(), &sorting)
                .await
                .is_err()
        );
        assert!(recorded(&db).is_empty());
        assert!(!db.seen_sms_path("/sms/1").unwrap());
        assert!(db.sorted_topic("promotional").unwrap().is_none());

        tg.fail.store(false, Ordering::SeqCst);
        handle_incoming(&db, "IR", sms, &tg, &bus(), &sorting)
            .await
            .unwrap();
        assert_eq!(tg.rich_posts.lock().unwrap().len(), 1);
        assert_eq!(recorded(&db).len(), 1);
    }

    #[tokio::test]
    async fn post_failure_in_existing_sorted_thread_leaves_sms_unrecorded() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_sorted_topic("promotional", 77, "تبلیغاتی")
            .unwrap();
        let tg = FakeTg::new();
        tg.fail.store(true, Ordering::SeqCst);
        let sorting = sorting_with(vec![Ok(stored("promotional", 0.9, &[]))]);

        assert!(handle_incoming(
            &db,
            "IR",
            unknown_sms("/sms/1", "off"),
            &tg,
            &bus(),
            &sorting
        )
        .await
        .is_err());
        assert!(recorded(&db).is_empty());
        assert!(!db.seen_sms_path("/sms/1").unwrap());
    }

    #[tokio::test]
    async fn stale_inbound_records_otp_code_without_posting() {
        let db = Db::open_in_memory().unwrap();
        db.add_otp_pattern(Some("bank"), r"رمز:\s+(\d{4,8})")
            .unwrap();
        let tg = FakeTg::new();
        let bus = bus();
        let mut events = bus.subscribe();
        let mut sms = unknown_sms("/sms/old", "رمز: 402332");
        sms.timestamp = "2024-12-21T15:12:23+03:30".into();
        let sorting = sorting_with(vec![]);

        handle_incoming(&db, "IR", sms, &tg, &bus, &sorting)
            .await
            .unwrap();

        assert!(tg.posts.lock().unwrap().is_empty());
        assert!(tg.rich_posts.lock().unwrap().is_empty());
        assert!(db.sorted_topic("otp").unwrap().is_none());
        assert_eq!(events.try_recv().unwrap().thread_id, GENERAL_THREAD);
        let rows = recorded(&db);
        assert_eq!(rows[0].otp_code.as_deref(), Some("402332"));
        assert!(rows[0].classification.is_none());
    }

    #[tokio::test]
    async fn html_special_chars_are_escaped() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();

        handle_incoming(
            &db,
            "IR",
            unknown_sms("/sms/1", "<b>x</b> & y"),
            &tg,
            &bus(),
            &Sorting::disabled(),
        )
        .await
        .unwrap();

        let posts = tg.rich_posts.lock().unwrap();
        assert!(posts[0].1.html.contains("&lt;b&gt;x&lt;/b&gt; &amp; y"));
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

    fn sweep_stuck_sms(ts: &str) -> IncomingSms {
        IncomingSms {
            path: "/sms/x".into(),
            e164: "Ewano".into(),
            text: String::new(),
            inbound: true,
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
            (
                "stuck receiving 7h",
                sweep_stuck_sms("2026-08-19T05:00:00+00:00"),
                false,
                SweepAction::Delete,
            ),
            (
                "stuck receiving 1h",
                sweep_stuck_sms("2026-08-19T11:00:00+00:00"),
                false,
                SweepAction::Keep,
            ),
            (
                "stuck receiving no ts",
                sweep_stuck_sms(""),
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

    #[derive(Clone)]
    enum FakeResult {
        Ok(usize),
        Transient,
        Auth,
        Json,
    }

    struct FakeSync {
        results: Mutex<VecDeque<FakeResult>>,
        calls: AtomicUsize,
    }

    impl FakeSync {
        fn new(results: Vec<FakeResult>) -> Self {
            Self {
                results: Mutex::new(results.into()),
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ContactsSync for FakeSync {
        async fn sync_all(&self, _db: &Db, _region: &str) -> Result<usize, GoogleError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut guard = self.results.lock().unwrap();
            if guard.is_empty() {
                return Ok(0);
            }
            let result = guard.front().unwrap().clone();
            if guard.len() > 1 {
                guard.pop_front();
            }
            match result {
                FakeResult::Ok(n) => Ok(n),
                FakeResult::Transient => Err(GoogleError::Transient("boom".into())),
                FakeResult::Auth => Err(GoogleError::Auth("dead".into())),
                FakeResult::Json => Err(GoogleError::Json(
                    serde_json::from_str::<serde_json::Value>("not json").unwrap_err(),
                )),
            }
        }
    }

    #[tokio::test]
    async fn sync_retries_transient_then_degrades() {
        let syncer = FakeSync::new(vec![FakeResult::Transient; 3]);
        let db = Db::open_in_memory().unwrap();
        let outcome = sync_with_retries(
            &syncer,
            &db,
            "IR",
            &[Duration::from_millis(1), Duration::from_millis(1)],
        )
        .await;
        assert_eq!(outcome, SyncOutcome::Degraded);
        assert_eq!(syncer.calls(), 3);
        assert!(db.contacts_available());
    }

    #[tokio::test]
    async fn sync_retries_transient_then_succeeds() {
        let syncer = FakeSync::new(vec![
            FakeResult::Transient,
            FakeResult::Transient,
            FakeResult::Ok(7),
        ]);
        let db = Db::open_in_memory().unwrap();
        let outcome = sync_with_retries(
            &syncer,
            &db,
            "IR",
            &[Duration::from_millis(1), Duration::from_millis(1)],
        )
        .await;
        assert_eq!(outcome, SyncOutcome::Ok);
        assert_eq!(syncer.calls(), 3);
    }

    #[tokio::test]
    async fn sync_auth_fails_fast_without_retry() {
        let syncer = FakeSync::new(vec![FakeResult::Auth]);
        let db = Db::open_in_memory().unwrap();
        let outcome = sync_with_retries(
            &syncer,
            &db,
            "IR",
            &[Duration::from_millis(1), Duration::from_millis(1)],
        )
        .await;
        assert_eq!(outcome, SyncOutcome::Auth);
        assert_eq!(syncer.calls(), 1);
    }

    #[tokio::test]
    async fn sync_other_errors_degrade_without_retry() {
        let syncer = FakeSync::new(vec![FakeResult::Json]);
        let db = Db::open_in_memory().unwrap();
        let outcome = sync_with_retries(
            &syncer,
            &db,
            "IR",
            &[Duration::from_millis(1), Duration::from_millis(1)],
        )
        .await;
        assert_eq!(outcome, SyncOutcome::Degraded);
        assert_eq!(syncer.calls(), 1);
    }

    #[tokio::test]
    async fn watch_contacts_alerts_once_on_token_death() {
        let syncer: Arc<dyn ContactsSync> = Arc::new(FakeSync::new(vec![FakeResult::Auth]));
        let db = Arc::new(Db::open_in_memory().unwrap());
        let tg = Arc::new(FakeTg::new());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(watch_contacts(
            syncer,
            db.clone(),
            "IR".into(),
            tg.clone(),
            Duration::from_millis(15),
            vec![Duration::from_millis(1), Duration::from_millis(1)],
            cancel.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(40)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("watch_contacts did not stop")
            .expect("watch_contacts join");
        assert!(!db.contacts_available());
        assert_eq!(
            tg.posts.lock().unwrap().as_slice(),
            &[(
                GENERAL_THREAD,
                "⚠️ Google contacts token dead — run cargo run -- auth".into()
            )]
        );
    }

    #[tokio::test]
    async fn watch_contacts_alerts_recovery_once() {
        let syncer: Arc<dyn ContactsSync> =
            Arc::new(FakeSync::new(vec![FakeResult::Auth, FakeResult::Ok(5)]));
        let db = Arc::new(Db::open_in_memory().unwrap());
        let tg = Arc::new(FakeTg::new());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(watch_contacts(
            syncer,
            db.clone(),
            "IR".into(),
            tg.clone(),
            Duration::from_millis(15),
            vec![Duration::from_millis(1), Duration::from_millis(1)],
            cancel.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(60)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("watch_contacts did not stop")
            .expect("watch_contacts join");
        assert!(db.contacts_available());
        assert_eq!(
            tg.posts.lock().unwrap().as_slice(),
            &[
                (
                    GENERAL_THREAD,
                    "⚠️ Google contacts token dead — run cargo run -- auth".into()
                ),
                (GENERAL_THREAD, "google contacts back".into()),
            ]
        );
    }

    #[tokio::test]
    async fn watch_contacts_reposts_alert_when_telegram_fails() {
        let syncer: Arc<dyn ContactsSync> = Arc::new(FakeSync::new(vec![FakeResult::Auth]));
        let db = Arc::new(Db::open_in_memory().unwrap());
        let tg = Arc::new(FakeTg {
            fail: AtomicBool::new(true),
            ..FakeTg::new()
        });
        let cancel = CancellationToken::new();
        let task = tokio::spawn(watch_contacts(
            syncer,
            db.clone(),
            "IR".into(),
            tg.clone(),
            Duration::from_millis(15),
            vec![Duration::from_millis(1), Duration::from_millis(1)],
            cancel.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(40)).await;
        tg.fail.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(40)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("watch_contacts did not stop")
            .expect("watch_contacts join");
        assert!(!db.contacts_available());
        assert_eq!(
            tg.posts.lock().unwrap().as_slice(),
            &[(
                GENERAL_THREAD,
                "⚠️ Google contacts token dead — run cargo run -- auth".into()
            )]
        );
    }

    #[tokio::test]
    async fn watch_contacts_transient_keeps_available_and_silent() {
        let syncer: Arc<dyn ContactsSync> = Arc::new(FakeSync::new(vec![FakeResult::Transient]));
        let db = Arc::new(Db::open_in_memory().unwrap());
        let tg = Arc::new(FakeTg::new());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(watch_contacts(
            syncer,
            db.clone(),
            "IR".into(),
            tg.clone(),
            Duration::from_millis(15),
            vec![Duration::from_millis(1), Duration::from_millis(1)],
            cancel.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(40)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("watch_contacts did not stop")
            .expect("watch_contacts join");
        assert!(db.contacts_available());
        assert!(tg.posts.lock().unwrap().is_empty());
    }
}
