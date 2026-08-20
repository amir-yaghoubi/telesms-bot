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
