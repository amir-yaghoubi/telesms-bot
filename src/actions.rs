use crate::app::TelegramSink;
use crate::db::{Contact, Db, Topic};
use crate::normalize::normalize_e164;
use crate::route::GENERAL_THREAD;

#[derive(Debug, Clone, Default)]
pub struct Identity {
    pub number: Option<String>,
    pub contact_id: Option<i64>,
    pub thread_id: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveMode {
    /// who / number: need a real topic
    RequireTopic,
    /// sms / ignore: unknown E.164 allowed
    AllowBareNumber,
    /// open: unknown number allowed (caller may create topic)
    Open,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub contact: Option<Contact>,
    pub topic: Option<Topic>,
    pub e164: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("{0}")]
    Validation(String),
    #[error("missing identity")]
    MissingIdentity,
    #[error("{0}")]
    InvalidNumber(String),
    #[error("{0}")]
    NotFound(String),
    #[error("identity fields disagree")]
    IdentityConflict,
    #[error("need default number")]
    NeedDefaultNumber { numbers: Vec<String> },
    #[error("contacts unavailable")]
    ContactsUnavailable,
    #[error("{0}")]
    ModemFailed(String),
    #[error("{0}")]
    ModemUnavailable(String),
    #[error("{0}")]
    ForwardFailed(String),
    #[error("{message}")]
    TelegramFailed { sent: bool, message: String },
    #[error(transparent)]
    Db(#[from] crate::db::DbError),
    #[error(transparent)]
    App(#[from] crate::app::AppError),
}

pub fn resolve(
    db: &Db,
    region: &str,
    id: &Identity,
    mode: ResolveMode,
) -> Result<Resolved, ActionError> {
    let e164 = match id
        .number
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw) => Some(
            normalize_e164(raw, region).map_err(|e| ActionError::InvalidNumber(e.to_string()))?,
        ),
        None => None,
    };

    if e164.is_none() && id.contact_id.is_none() && id.thread_id.is_none() {
        return Err(ActionError::MissingIdentity);
    }

    let mut contact: Option<Contact> = None;
    let mut topic: Option<Topic> = None;
    let mut contact_ids = Vec::<i64>::new();
    let mut topic_ids = Vec::<i32>::new();

    if let Some(cid) = id.contact_id {
        let c = db
            .get_contact(cid)?
            .ok_or_else(|| ActionError::NotFound("unknown contact".into()))?;
        contact_ids.push(c.id);
        contact = Some(c);
    }

    if let Some(tid) = id.thread_id {
        if tid == GENERAL_THREAD && mode == ResolveMode::RequireTopic {
            return Err(ActionError::NotFound("unknown topic".into()));
        }
        match db.get_topic_by_thread(tid)? {
            Some(t) => {
                topic_ids.push(t.thread_id);
                topic = Some(t);
            }
            None if mode == ResolveMode::RequireTopic => {
                return Err(ActionError::NotFound("unknown topic".into()));
            }
            None => {}
        }
    }

    if let Some(ref e) = e164 {
        if let Some(c) = db.find_contact_by_e164(e)? {
            contact_ids.push(c.id);
            contact = Some(c);
        }

        let t = match db.get_topic_by_e164(e)? {
            Some(t) => Some(t),
            None => contact
                .as_ref()
                .and_then(|c| db.get_topic_by_contact(c.id).transpose())
                .transpose()?,
        };
        if let Some(t) = t {
            topic_ids.push(t.thread_id);
            topic = Some(t);
        }
    }

    if let Some(ref c) = contact {
        if let Some(t) = db.get_topic_by_contact(c.id)? {
            topic_ids.push(t.thread_id);
            if topic.is_none() {
                topic = Some(t);
            }
        }
    }

    if let Some(ref t) = topic {
        if let Some(cid) = t.contact_id {
            contact_ids.push(cid);
        }
    }

    if contact_ids.iter().copied().collect::<std::collections::HashSet<_>>().len() > 1 {
        return Err(ActionError::IdentityConflict);
    }
    if topic_ids.iter().copied().collect::<std::collections::HashSet<_>>().len() > 1 {
        return Err(ActionError::IdentityConflict);
    }
    if let (Some(provided), Some(ref t)) = (id.thread_id, topic.as_ref()) {
        if provided != t.thread_id {
            return Err(ActionError::IdentityConflict);
        }
    }

    if contact.is_none() {
        if let Some(ref t) = topic {
            if let Some(cid) = t.contact_id {
                contact = db.get_contact(cid)?;
            }
        }
    }

    if mode == ResolveMode::RequireTopic && topic.is_none() {
        return Err(ActionError::NotFound("unknown topic".into()));
    }

    Ok(Resolved {
        contact,
        topic,
        e164,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatList {
    pub chats: Vec<ChatListItem>,
    pub next_before: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatListItem {
    pub thread_id: i32,
    pub title: String,
    pub contact_id: Option<i64>,
    pub display_name: Option<String>,
    pub default_e164: Option<String>,
    pub last_message_at: String,
    pub last_message_preview: String,
    pub last_message_direction: String,
    pub unread_count: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MessageList {
    pub thread_id: i32,
    pub title: String,
    pub contact_id: Option<i64>,
    pub messages: Vec<MessageItem>,
    pub next_before: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MessageItem {
    pub id: String,
    pub direction: String,
    pub e164: String,
    pub body: String,
    pub timestamp: String,
    pub sms_ts: Option<String>,
    pub status: String,
}

fn history_limit(limit: Option<i64>) -> Result<i64, ActionError> {
    let limit = limit.unwrap_or(50);
    if !(1..=100).contains(&limit) {
        return Err(ActionError::Validation("limit must be 1..=100".into()));
    }
    Ok(limit)
}

fn parse_history_cursor(raw: Option<&str>) -> Result<Option<String>, ActionError> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let parsed = chrono::DateTime::parse_from_rfc3339(raw)
        .map_err(|_| ActionError::Validation("invalid cursor timestamp".into()))?;
    Ok(Some(parsed.with_timezone(&chrono::Utc).to_rfc3339()))
}

fn history_cursors(
    before: Option<&str>,
    after: Option<&str>,
) -> Result<(Option<String>, Option<String>), ActionError> {
    let before = parse_history_cursor(before)?;
    let after = parse_history_cursor(after)?;
    if let (Some(before), Some(after)) = (&before, &after) {
        let before = chrono::DateTime::parse_from_rfc3339(before)
            .expect("history cursor was already validated");
        let after = chrono::DateTime::parse_from_rfc3339(after)
            .expect("history cursor was already validated");
        if before <= after {
            return Err(ActionError::Validation(
                "before must be greater than after".into(),
            ));
        }
    }
    Ok((before, after))
}

pub fn list_chats(
    db: &Db,
    limit: Option<i64>,
    before: Option<&str>,
    after: Option<&str>,
) -> Result<ChatList, ActionError> {
    let limit = history_limit(limit)?;
    let (before, after) = history_cursors(before, after)?;
    let chats = db
        .chats_with_activity(limit, before.as_deref(), after.as_deref())?
        .into_iter()
        .map(|chat| ChatListItem {
            thread_id: chat.thread_id,
            title: chat.title,
            contact_id: chat.contact_id,
            display_name: chat.display_name,
            default_e164: chat.default_e164,
            last_message_at: chat.last_message_at,
            last_message_preview: chat.last_message_preview,
            last_message_direction: chat.last_message_direction,
            unread_count: None,
        })
        .collect::<Vec<_>>();
    let next_before = (chats.len() == limit as usize)
        .then(|| chats.last().map(|chat| chat.last_message_at.clone()))
        .flatten();
    Ok(ChatList { chats, next_before })
}

fn check_message_identity(
    db: &Db,
    region: &str,
    thread_id: i32,
    topic: Option<&Topic>,
    number: Option<&str>,
    contact_id: Option<i64>,
) -> Result<(), ActionError> {
    if thread_id == GENERAL_THREAD {
        return if contact_id.is_some() {
            Err(ActionError::IdentityConflict)
        } else {
            Ok(())
        };
    }

    let topic = topic.expect("non-General thread was checked before identity validation");
    if contact_id.is_some_and(|provided| topic.contact_id != Some(provided)) {
        return Err(ActionError::IdentityConflict);
    }

    let Some(number) = number else {
        return Ok(());
    };
    let e164 = normalize_e164(number, region)
        .map_err(|error| ActionError::InvalidNumber(error.to_string()))?;
    if topic.default_e164.as_deref() == Some(e164.as_str()) {
        return Ok(());
    }
    if let Some(contact_id) = topic.contact_id {
        if db.contact_numbers(contact_id)?.contains(&e164) {
            return Ok(());
        }
    }
    Err(ActionError::IdentityConflict)
}

#[allow(clippy::too_many_arguments)]
pub fn list_messages(
    db: &Db,
    region: &str,
    thread_id: i32,
    limit: Option<i64>,
    before: Option<&str>,
    after: Option<&str>,
    number: Option<&str>,
    contact_id: Option<i64>,
) -> Result<MessageList, ActionError> {
    let limit = history_limit(limit)?;
    let (before, after) = history_cursors(before, after)?;
    let topic = if thread_id == GENERAL_THREAD {
        None
    } else {
        Some(
            db.get_topic_by_thread(thread_id)?
                .ok_or_else(|| ActionError::NotFound("unknown thread".into()))?,
        )
    };
    check_message_identity(db, region, thread_id, topic.as_ref(), number, contact_id)?;

    let messages = db
        .messages_for_thread(thread_id, limit, before.as_deref(), after.as_deref())?
        .into_iter()
        .map(|message| MessageItem {
            id: message.id,
            direction: message.direction,
            e164: message.e164,
            body: message.body,
            timestamp: message.timestamp,
            sms_ts: message.sms_ts,
            status: message.status,
        })
        .collect::<Vec<_>>();
    let next_before = (messages.len() == limit as usize)
        .then(|| messages.last().map(|message| message.timestamp.clone()))
        .flatten();
    let (title, topic_contact_id) = topic
        .map(|topic| (topic.title, topic.contact_id))
        .unwrap_or_else(|| ("General".into(), None));

    Ok(MessageList {
        thread_id,
        title,
        contact_id: topic_contact_id,
        messages,
        next_before,
    })
}

pub const SEARCH_LIMIT: usize = 20;

pub fn search_contacts(db: &Db, query: &str) -> Result<Vec<Contact>, ActionError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(ActionError::Validation("query required".into()));
    }
    match db.search_contacts(query) {
        Ok(mut hits) => {
            hits.truncate(SEARCH_LIMIT);
            Ok(hits)
        }
        Err(crate::db::DbError::ContactsUnavailable) => Err(ActionError::ContactsUnavailable),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, Clone)]
pub struct Who {
    pub thread_id: i32,
    pub contact_id: Option<i64>,
    pub display_name: String,
    pub numbers: Vec<String>,
    pub default_e164: Option<String>,
    pub ambiguous: bool,
}

pub fn who(db: &Db, region: &str, id: &Identity) -> Result<Who, ActionError> {
    let resolved = resolve(db, region, id, ResolveMode::RequireTopic)?;
    let topic = resolved
        .topic
        .as_ref()
        .ok_or_else(|| ActionError::NotFound("unknown topic".into()))?;
    let numbers = if let Some(contact_id) = topic.contact_id {
        db.contact_numbers(contact_id)?
    } else {
        topic.default_e164.clone().into_iter().collect()
    };
    let (display_name, ambiguous) = match resolved.contact {
        Some(ref c) => (c.display_name.clone(), c.ambiguous),
        None => (topic.title.clone(), false),
    };
    Ok(Who {
        thread_id: topic.thread_id,
        contact_id: topic.contact_id,
        display_name,
        numbers,
        default_e164: topic.default_e164.clone(),
        ambiguous,
    })
}

#[derive(Debug, Clone)]
pub struct NumberState {
    pub thread_id: i32,
    pub numbers: Vec<String>,
    pub default_e164: Option<String>,
}

fn topic_numbers(db: &Db, topic: &Topic) -> Result<Vec<String>, ActionError> {
    if let Some(contact_id) = topic.contact_id {
        Ok(db.contact_numbers(contact_id)?)
    } else {
        Ok(topic.default_e164.clone().into_iter().collect())
    }
}

pub fn list_numbers(db: &Db, region: &str, id: &Identity) -> Result<NumberState, ActionError> {
    let w = who(db, region, id)?;
    Ok(NumberState {
        thread_id: w.thread_id,
        numbers: w.numbers,
        default_e164: w.default_e164,
    })
}

pub async fn set_default_number(
    db: &Db,
    region: &str,
    id: &Identity,
    new_default: &str,
    tg: &dyn TelegramSink,
) -> Result<NumberState, ActionError> {
    let resolved = resolve(db, region, id, ResolveMode::RequireTopic)?;
    let topic = resolved
        .topic
        .as_ref()
        .ok_or_else(|| ActionError::NotFound("unknown topic".into()))?;
    let e164 = normalize_e164(new_default, region)
        .map_err(|e| ActionError::InvalidNumber(e.to_string()))?;
    let numbers = topic_numbers(db, topic)?;
    if !numbers.contains(&e164) {
        return Err(ActionError::Validation("number not on this topic".into()));
    }
    db.set_default_number(topic.thread_id, &e164)?;
    tg.post(topic.thread_id, format!("default is {e164}"))
        .await
        .map_err(ActionError::from)?;
    Ok(NumberState {
        thread_id: topic.thread_id,
        numbers,
        default_e164: Some(e164),
    })
}

pub async fn ignore(
    db: &Db,
    region: &str,
    id: &Identity,
    tg: &dyn TelegramSink,
) -> Result<Vec<String>, ActionError> {
    let has_number = id
        .number
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some();

    if has_number {
        let resolved = resolve(db, region, id, ResolveMode::AllowBareNumber)?;
        let e164 = resolved
            .e164
            .ok_or_else(|| ActionError::InvalidNumber("missing number".into()))?;
        db.ignore_number(&e164)?;
        if let Some(topic) = resolved.topic {
            tg.post(topic.thread_id, format!("ignored {e164}"))
                .await
                .map_err(ActionError::from)?;
        }
        return Ok(vec![e164]);
    }

    let resolved = resolve(db, region, id, ResolveMode::RequireTopic)?;
    let topic = resolved
        .topic
        .as_ref()
        .ok_or_else(|| ActionError::NotFound("unknown topic".into()))?;
    let mut targets = topic_numbers(db, topic)?;
    if targets.is_empty() {
        if let Some(e164) = &topic.default_e164 {
            targets.push(e164.clone());
        }
    }
    if targets.is_empty() {
        return Err(ActionError::Validation(
            "reply to a +number to ignore it".into(),
        ));
    }
    for n in &targets {
        db.ignore_number(n)?;
    }
    tg.post(
        topic.thread_id,
        format!("ignored {}", targets.join(", ")),
    )
    .await
    .map_err(ActionError::from)?;
    Ok(targets)
}

#[derive(Debug, Clone)]
pub struct Opened {
    pub contact_id: Option<i64>,
    pub thread_id: i32,
    pub title: String,
    pub created: bool,
}

pub async fn open_topic(
    db: &Db,
    region: &str,
    id: &Identity,
    tg: &dyn TelegramSink,
) -> Result<Opened, ActionError> {
    let id = Identity {
        thread_id: None,
        ..id.clone()
    };
    let resolved = resolve(db, region, &id, ResolveMode::Open)?;

    if let Some(topic) = resolved.topic {
        return Ok(Opened {
            contact_id: topic.contact_id,
            thread_id: topic.thread_id,
            title: topic.title,
            created: false,
        });
    }

    if let Some(contact) = resolved.contact {
        let default_e164 = if contact.numbers.len() == 1 {
            contact.numbers.first().cloned()
        } else {
            None
        };
        let title = match contact.numbers.first() {
            Some(n) => crate::route::topic_title(&contact.display_name, n),
            None => contact.display_name.clone(),
        };
        let thread_id = tg.create_topic(title.clone()).await?;
        db.upsert_topic(&Topic {
            thread_id,
            contact_id: Some(contact.id),
            default_e164,
            title: title.clone(),
            ignored: false,
        })?;
        return Ok(Opened {
            contact_id: Some(contact.id),
            thread_id,
            title,
            created: true,
        });
    }

    if let Some(e164) = resolved.e164 {
        let title = e164.clone();
        let thread_id = tg.create_topic(title.clone()).await?;
        db.upsert_topic(&Topic {
            thread_id,
            contact_id: None,
            default_e164: Some(e164),
            title: title.clone(),
            ignored: false,
        })?;
        return Ok(Opened {
            contact_id: None,
            thread_id,
            title,
            created: true,
        });
    }

    Err(ActionError::NotFound("unknown contact".into()))
}

#[derive(Debug, Clone)]
pub struct SmsSent {
    pub e164: String,
    pub thread_id: i32,
    pub sent: bool,
}

async fn resolve_send_thread(
    db: &Db,
    e164: &str,
    reply_thread: i32,
    tg: &dyn TelegramSink,
) -> Result<i32, ActionError> {
    use crate::route::{route_for_send, InboundDest};

    Ok(match route_for_send(db, e164)? {
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
            thread_id
        }
        InboundDest::ExistingTopic { thread_id, .. } => thread_id,
        InboundDest::General { e164: dest } => {
            if db.is_ignored(&dest)? {
                reply_thread
            } else {
                let thread_id = tg.create_topic(dest.clone()).await?;
                db.upsert_topic(&Topic {
                    thread_id,
                    contact_id: None,
                    default_e164: Some(dest.clone()),
                    title: dest,
                    ignored: false,
                })?;
                thread_id
            }
        }
    })
}

pub async fn send_sms(
    db: &Db,
    region: &str,
    id: &Identity,
    text: &str,
    reply_thread: i32,
    reply_to: Option<i32>,
    modem: &dyn crate::modem::SmsModem,
    tg: &dyn TelegramSink,
    delete_enabled: bool,
) -> Result<SmsSent, ActionError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ActionError::Validation("text required".into()));
    }

    if id
        .number
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some()
    {
        let resolved = resolve(db, region, id, ResolveMode::AllowBareNumber)?;
        let e164 = resolved
            .e164
            .ok_or_else(|| ActionError::InvalidNumber("missing number".into()))?;
        let thread_id = resolve_send_thread(db, &e164, reply_thread, tg).await?;
        crate::app::send_and_ack(
            db,
            &e164,
            text,
            thread_id,
            reply_to,
            modem,
            tg,
            delete_enabled,
        )
        .await?;
        return Ok(SmsSent {
            e164,
            thread_id,
            sent: true,
        });
    }

    if id.contact_id.is_none() {
        return Err(ActionError::MissingIdentity);
    }
    let resolved = resolve(db, region, id, ResolveMode::RequireTopic)?;
    let topic = resolved
        .topic
        .as_ref()
        .ok_or_else(|| ActionError::NotFound("unknown topic".into()))?;
    let e164 = match &topic.default_e164 {
        Some(e164) => e164.clone(),
        None => {
            let numbers = topic_numbers(db, topic)?;
            return Err(ActionError::NeedDefaultNumber { numbers });
        }
    };
    let thread_id = topic.thread_id;
    crate::app::send_and_ack(
        db,
        &e164,
        text,
        thread_id,
        reply_to,
        modem,
        tg,
        delete_enabled,
    )
    .await?;
    Ok(SmsSent {
        e164,
        thread_id,
        sent: true,
    })
}

pub async fn get_call_forward(
    forward: &dyn crate::modem::CallForward,
    region: &str,
) -> Result<crate::call_forward::CallForwardState, ActionError> {
    forward
        .query_forward(region)
        .await
        .map_err(map_forward_err)
}

pub async fn put_call_forward(
    forward: &dyn crate::modem::CallForward,
    region: &str,
    e164: Option<String>,
) -> Result<crate::call_forward::CallForwardState, ActionError> {
    match e164 {
        None => forward
            .disable_forward(region)
            .await
            .map_err(map_forward_err),
        Some(raw) => {
            let e164 = normalize_e164(&raw, region)
                .map_err(|e| ActionError::InvalidNumber(e.to_string()))?;
            forward
                .set_forward(&e164, region)
                .await
                .map_err(map_forward_err)
        }
    }
}

fn map_forward_err(err: crate::modem::ModemError) -> ActionError {
    match err {
        crate::modem::ModemError::NotFound(msg) => ActionError::ModemUnavailable(msg),
        crate::modem::ModemError::Failed(msg) => ActionError::ForwardFailed(msg),
    }
}

pub async fn status(
    modem: &dyn crate::modem::ModemInfo,
    forward: &dyn crate::modem::CallForward,
    region: &str,
    db: &Db,
    tz: chrono_tz::Tz,
    modem_uid: &str,
) -> Result<serde_json::Value, ActionError> {
    let snap = crate::status::gather(
        modem,
        forward,
        region,
        db,
        tz,
        modem_uid,
        chrono::Utc::now(),
    )
    .await?;
    Ok(serde_json::to_value(crate::status::status_json_from_snapshot(&snap))
        .expect("StatusJson serializes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, Topic};
    use crate::route::GENERAL_THREAD;

    fn seed() -> (Db, i64) {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        db.upsert_topic(&Topic {
            thread_id: 9,
            contact_id: Some(id),
            default_e164: Some("+989121234567".into()),
            title: "Ali".into(),
            ignored: false,
        })
        .unwrap();
        (db, id)
    }

    #[test]
    fn list_chats_empty_ok() {
        let db = Db::open_in_memory().unwrap();
        let out = list_chats(&db, None, None, None).unwrap();
        assert!(out.chats.is_empty());
        assert!(out.next_before.is_none());
    }

    #[test]
    fn list_chats_full_page_sets_next_before() {
        let db = Db::open_in_memory().unwrap();
        db.insert_inbound_at(
            "/g",
            "+98912",
            "hello",
            "2026-08-20T08:00:00Z",
            Some(GENERAL_THREAD),
        )
        .unwrap();

        let out = list_chats(&db, Some(1), None, None).unwrap();

        assert_eq!(out.chats.len(), 1);
        assert_eq!(out.next_before.as_deref(), Some("2026-08-20T08:00:00Z"));
        assert!(out.chats[0].unread_count.is_none());
    }

    #[test]
    fn list_chats_offset_cursor_matches_utc_equivalent() {
        let db = Db::open_in_memory().unwrap();
        for (thread_id, e164, title) in
            [(41, "+98912", "Before"), (42, "+98913", "After")]
        {
            db.upsert_topic(&Topic {
                thread_id,
                contact_id: None,
                default_e164: Some(e164.into()),
                title: title.into(),
                ignored: false,
            })
            .unwrap();
        }
        db.insert_inbound_at(
            "/before",
            "+98912",
            "before",
            "2026-08-20T08:30:00Z",
            Some(41),
        )
        .unwrap();
        db.insert_inbound_at(
            "/after",
            "+98913",
            "after",
            "2026-08-20T09:30:00Z",
            Some(42),
        )
        .unwrap();

        let utc = list_chats(&db, None, Some("2026-08-20T09:00:00Z"), None).unwrap();
        let offset =
            list_chats(&db, None, Some("2026-08-20T12:30:00+03:30"), None).unwrap();

        assert_eq!(offset.chats.len(), utc.chats.len());
        assert_eq!(
            offset
                .chats
                .iter()
                .map(|chat| chat.last_message_preview.as_str())
                .collect::<Vec<_>>(),
            utc.chats
                .iter()
                .map(|chat| chat.last_message_preview.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(utc.chats[0].last_message_preview, "before");
    }

    #[test]
    fn list_messages_unknown_thread_404() {
        let db = Db::open_in_memory().unwrap();
        let err = list_messages(&db, "IR", 99, None, None, None, None, None).unwrap_err();
        assert!(matches!(err, ActionError::NotFound(_)));
    }

    #[test]
    fn list_messages_general_without_topic_ok() {
        let db = Db::open_in_memory().unwrap();
        db.record_inbound("/g", "+98912", "x", None, "", GENERAL_THREAD)
            .unwrap();

        let out = list_messages(&db, "IR", GENERAL_THREAD, None, None, None, None, None).unwrap();

        assert_eq!(out.thread_id, GENERAL_THREAD);
        assert_eq!(out.title, "General");
        assert_eq!(out.contact_id, None);
        assert_eq!(out.messages.len(), 1);
    }

    #[test]
    fn list_messages_general_rejects_contact_id() {
        let db = Db::open_in_memory().unwrap();
        let err =
            list_messages(&db, "IR", GENERAL_THREAD, None, None, None, None, Some(7)).unwrap_err();
        assert!(matches!(err, ActionError::IdentityConflict));
    }

    #[test]
    fn list_messages_general_allows_number() {
        let db = Db::open_in_memory().unwrap();
        let out = list_messages(
            &db,
            "IR",
            GENERAL_THREAD,
            None,
            None,
            None,
            Some("09121234567"),
            None,
        )
        .unwrap();
        assert_eq!(out.title, "General");
    }

    #[test]
    fn list_messages_identity_conflict() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: Some(id),
            default_e164: Some("+989121111111".into()),
            title: "Ali".into(),
            ignored: false,
        })
        .unwrap();
        let err = list_messages(&db, "IR", 42, None, None, None, None, Some(id + 1)).unwrap_err();
        assert!(matches!(err, ActionError::IdentityConflict));
    }

    #[test]
    fn list_messages_accepts_contacts_secondary_number() {
        let (db, id) = seed();
        db.replace_contact_numbers(id, &["+989121234567".into(), "+989131234567".into()])
            .unwrap();

        let out = list_messages(
            &db,
            "IR",
            9,
            None,
            None,
            None,
            Some("09131234567"),
            Some(id),
        )
        .unwrap();

        assert_eq!(out.contact_id, Some(id));
    }

    #[test]
    fn list_messages_rejects_foreign_number() {
        let (db, id) = seed();
        let err = list_messages(
            &db,
            "IR",
            9,
            None,
            None,
            None,
            Some("09139999999"),
            Some(id),
        )
        .unwrap_err();
        assert!(matches!(err, ActionError::IdentityConflict));
    }

    #[test]
    fn list_messages_number_only_topic_requires_default_match() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: None,
            default_e164: Some("+989121111111".into()),
            title: "Unknown".into(),
            ignored: false,
        })
        .unwrap();

        let err =
            list_messages(&db, "IR", 42, None, None, None, Some("09122222222"), None).unwrap_err();

        assert!(matches!(err, ActionError::IdentityConflict));
    }

    #[test]
    fn list_messages_maps_rows_and_sets_next_before() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: None,
            default_e164: Some("+98912".into()),
            title: "Unknown".into(),
            ignored: false,
        })
        .unwrap();
        db.insert_inbound_at("/m", "+98912", "hello", "2026-08-20T08:00:00Z", Some(42))
            .unwrap();

        let out = list_messages(&db, "IR", 42, Some(1), None, None, None, None).unwrap();

        assert_eq!(out.messages.len(), 1);
        assert_eq!(out.messages[0].body, "hello");
        assert_eq!(out.messages[0].direction, "in");
        assert_eq!(out.next_before.as_deref(), Some("2026-08-20T08:00:00Z"));
    }

    #[test]
    fn bad_limit_validation() {
        let db = Db::open_in_memory().unwrap();
        let err = list_chats(&db, Some(0), None, None).unwrap_err();
        assert!(matches!(err, ActionError::Validation(_)));
    }

    #[test]
    fn invalid_cursor_validation() {
        let db = Db::open_in_memory().unwrap();
        let err = list_chats(&db, None, Some("tomorrow"), None).unwrap_err();
        assert!(matches!(err, ActionError::Validation(_)));
    }

    #[test]
    fn before_not_after_after_validation() {
        let db = Db::open_in_memory().unwrap();
        let err = list_chats(
            &db,
            None,
            Some("2026-08-01T00:00:00Z"),
            Some("2026-08-02T00:00:00Z"),
        )
        .unwrap_err();
        assert!(matches!(err, ActionError::Validation(_)));
    }

    #[test]
    fn number_only_resolves() {
        let (db, id) = seed();
        let r = resolve(
            &db,
            "IR",
            &Identity {
                number: Some("09121234567".into()),
                ..Default::default()
            },
            ResolveMode::RequireTopic,
        )
        .unwrap();
        assert_eq!(r.contact.unwrap().id, id);
        assert_eq!(r.topic.unwrap().thread_id, 9);
        assert_eq!(r.e164.as_deref(), Some("+989121234567"));
    }

    #[test]
    fn contact_only_resolves() {
        let (db, id) = seed();
        let r = resolve(
            &db,
            "IR",
            &Identity {
                contact_id: Some(id),
                ..Default::default()
            },
            ResolveMode::RequireTopic,
        )
        .unwrap();
        assert_eq!(r.topic.unwrap().thread_id, 9);
    }

    #[test]
    fn matching_pair_ok() {
        let (db, id) = seed();
        resolve(
            &db,
            "IR",
            &Identity {
                number: Some("09121234567".into()),
                contact_id: Some(id),
                thread_id: Some(9),
            },
            ResolveMode::RequireTopic,
        )
        .unwrap();
    }

    #[test]
    fn conflicting_contact_and_number() {
        let (db, id) = seed();
        let other = db.upsert_contact("people/b", "Bob").unwrap();
        db.replace_contact_numbers(other, &["+989130000000".into()])
            .unwrap();
        let err = resolve(
            &db,
            "IR",
            &Identity {
                number: Some("09121234567".into()),
                contact_id: Some(other),
                ..Default::default()
            },
            ResolveMode::RequireTopic,
        )
        .unwrap_err();
        assert!(matches!(err, ActionError::IdentityConflict));
        let _ = id;
    }

    #[test]
    fn bad_number() {
        let db = Db::open_in_memory().unwrap();
        let err = resolve(
            &db,
            "IR",
            &Identity {
                number: Some("not-a-number".into()),
                ..Default::default()
            },
            ResolveMode::AllowBareNumber,
        )
        .unwrap_err();
        assert!(matches!(err, ActionError::InvalidNumber(_)));
    }

    #[test]
    fn thread_alone_unknown_topic() {
        let db = Db::open_in_memory().unwrap();
        let err = resolve(
            &db,
            "IR",
            &Identity {
                thread_id: Some(9),
                ..Default::default()
            },
            ResolveMode::RequireTopic,
        )
        .unwrap_err();
        assert!(matches!(err, ActionError::NotFound(_)));
    }

    #[test]
    fn general_thread_require_topic() {
        let (db, id) = seed();
        let err = resolve(
            &db,
            "IR",
            &Identity {
                contact_id: Some(id),
                thread_id: Some(GENERAL_THREAD),
                ..Default::default()
            },
            ResolveMode::RequireTopic,
        )
        .unwrap_err();
        assert!(matches!(err, ActionError::NotFound(_)));
    }

    #[test]
    fn unknown_number_allowed_for_sms() {
        let db = Db::open_in_memory().unwrap();
        let r = resolve(
            &db,
            "IR",
            &Identity {
                number: Some("09120000000".into()),
                ..Default::default()
            },
            ResolveMode::AllowBareNumber,
        )
        .unwrap();
        assert!(r.contact.is_none());
        assert!(r.topic.is_none());
        assert!(r.e164.unwrap().starts_with('+'));
    }

    #[test]
    fn search_empty_query() {
        let db = Db::open_in_memory().unwrap();
        assert!(matches!(
            search_contacts(&db, "  ").unwrap_err(),
            ActionError::Validation(_)
        ));
    }

    #[test]
    fn search_returns_hit_and_caps() {
        let db = Db::open_in_memory().unwrap();
        for i in 0..25 {
            db.upsert_contact(&format!("people/{i}"), "Ali").unwrap();
        }
        let hits = search_contacts(&db, "Ali").unwrap();
        assert_eq!(hits.len(), 20);
    }

    #[test]
    fn search_unavailable() {
        let db = Db::open_in_memory().unwrap();
        db.set_contacts_available(false);
        assert!(matches!(
            search_contacts(&db, "x").unwrap_err(),
            ActionError::ContactsUnavailable
        ));
    }

    #[test]
    fn who_lists_default() {
        let (db, id) = seed();
        let w = who(
            &db,
            "IR",
            &Identity {
                contact_id: Some(id),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(w.thread_id, 9);
        assert_eq!(w.default_e164.as_deref(), Some("+989121234567"));
        assert!(w.numbers.contains(&"+989121234567".into()));
    }

    #[tokio::test]
    async fn set_default_rejects_foreign_number() {
        let (db, id) = seed();
        let tg = crate::app::FakeTg::new();
        let err = set_default_number(
            &db,
            "IR",
            &Identity {
                contact_id: Some(id),
                ..Default::default()
            },
            "09139999999",
            &tg,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ActionError::Validation(_)));
    }

    #[tokio::test]
    async fn set_default_posts_and_persists() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+98912".into(), "+98913".into()])
            .unwrap();
        db.upsert_topic(&Topic {
            thread_id: 9,
            contact_id: Some(id),
            default_e164: Some("+98912".into()),
            title: "Ali".into(),
            ignored: false,
        })
        .unwrap();
        let tg = crate::app::FakeTg::new();
        let st = set_default_number(
            &db,
            "IR",
            &Identity {
                contact_id: Some(id),
                ..Default::default()
            },
            "+98913",
            &tg,
        )
        .await
        .unwrap();
        assert_eq!(st.default_e164.as_deref(), Some("+98913"));
        assert_eq!(
            tg.posts.lock().unwrap().as_slice(),
            &[(9, "default is +98913".into())]
        );
    }

    #[tokio::test]
    async fn open_existing_does_not_create() {
        let (db, id) = seed();
        let tg = crate::app::FakeTg::new();
        let o = open_topic(
            &db,
            "IR",
            &Identity {
                contact_id: Some(id),
                ..Default::default()
            },
            &tg,
        )
        .await
        .unwrap();
        assert!(!o.created);
        assert_eq!(o.thread_id, 9);
    }

    #[tokio::test]
    async fn open_creates_for_contact() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        let tg = crate::app::FakeTg::new();
        let o = open_topic(
            &db,
            "IR",
            &Identity {
                contact_id: Some(id),
                ..Default::default()
            },
            &tg,
        )
        .await
        .unwrap();
        assert!(o.created);
        assert!(db.get_topic_by_contact(id).unwrap().is_some());
    }

    #[tokio::test]
    async fn open_unknown_number_creates_title() {
        let db = Db::open_in_memory().unwrap();
        let tg = crate::app::FakeTg::new();
        let o = open_topic(
            &db,
            "IR",
            &Identity {
                number: Some("09120000000".into()),
                ..Default::default()
            },
            &tg,
        )
        .await
        .unwrap();
        assert!(o.created);
        assert!(o.title.starts_with('+'));
    }

    #[tokio::test]
    async fn send_by_number_creates_and_acks() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        let tg = crate::app::FakeTg::new();
        let modem = crate::modem::FakeModem::default();
        let s = send_sms(
            &db,
            "IR",
            &Identity {
                number: Some("09121234567".into()),
                ..Default::default()
            },
            "hello",
            1,
            Some(7),
            &modem,
            &tg,
            true,
        )
        .await
        .unwrap();
        assert_eq!(s.e164, "+989121234567");
        assert_eq!(
            modem.sent.lock().unwrap().as_slice(),
            &[("+989121234567".into(), "hello".into())]
        );
        assert_eq!(
            tg.replies.lock().unwrap().last().map(|p| p.1.as_str()),
            Some("✅")
        );
    }

    #[tokio::test]
    async fn send_to_default_need_number() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+98912".into(), "+98913".into()])
            .unwrap();
        db.upsert_topic(&Topic {
            thread_id: 9,
            contact_id: Some(id),
            default_e164: None,
            title: "Ali".into(),
            ignored: false,
        })
        .unwrap();
        let tg = crate::app::FakeTg::new();
        let modem = crate::modem::FakeModem::default();
        let err = send_sms(
            &db,
            "IR",
            &Identity {
                contact_id: Some(id),
                ..Default::default()
            },
            "hi",
            9,
            None,
            &modem,
            &tg,
            true,
        )
        .await
        .unwrap_err();
        match err {
            ActionError::NeedDefaultNumber { numbers } => {
                assert_eq!(numbers.len(), 2);
            }
            other => panic!("{other:?}"),
        }
        assert!(modem.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn send_modem_fail_is_error() {
        let db = Db::open_in_memory().unwrap();
        let tg = crate::app::FakeTg::new();
        let mut modem = crate::modem::FakeModem::default();
        modem.fail = true;
        let err = send_sms(
            &db,
            "IR",
            &Identity {
                number: Some("09121234567".into()),
                ..Default::default()
            },
            "x",
            1,
            None,
            &modem,
            &tg,
            true,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ActionError::ModemFailed(_)));
    }

    #[tokio::test]
    async fn status_offline_json() {
        let db = Db::open_in_memory().unwrap();
        let modem = crate::modem::FakeModem::default();
        let v = status(&modem, &modem, "IR", &db, chrono_tz::UTC, "dwm222")
            .await
            .unwrap();
        assert_eq!(v["modem"]["state"], "offline");
        assert_eq!(v["modem_uid"], "dwm222");
        assert_eq!(v["contacts_ok"], true);
        assert!(v.get("forward").is_none());
    }

    #[tokio::test]
    async fn put_call_forward_set_and_disable() {
        let m = crate::modem::FakeModem::default();
        let on = put_call_forward(&m, "IR", Some("09121234567".into()))
            .await
            .unwrap();
        assert_eq!(on.e164.as_deref(), Some("+989121234567"));
        let off = put_call_forward(&m, "IR", None).await.unwrap();
        assert!(!off.enabled);
    }

    #[tokio::test]
    async fn get_call_forward_maps_modem_fail() {
        let m = crate::modem::FakeModem {
            forward_fail: true,
            ..Default::default()
        };
        let err = get_call_forward(&m, "IR").await.unwrap_err();
        assert!(matches!(err, ActionError::ForwardFailed(_)));
    }

    #[tokio::test]
    async fn ignore_by_number_without_topic_skips_telegram() {
        let db = Db::open_in_memory().unwrap();
        let tg = crate::app::FakeTg::new();
        let ignored = ignore(
            &db,
            "IR",
            &Identity {
                number: Some("09121234567".into()),
                ..Default::default()
            },
            &tg,
        )
        .await
        .unwrap();
        assert_eq!(ignored, vec!["+989121234567".to_string()]);
        assert!(tg.posts.lock().unwrap().is_empty());
        assert!(db.is_ignored("+989121234567").unwrap());
    }
}
