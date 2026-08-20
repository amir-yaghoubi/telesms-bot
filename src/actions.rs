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

    if e164.is_none() && id.contact_id.is_none() {
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
    fn thread_alone_missing_identity() {
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
        assert!(matches!(err, ActionError::MissingIdentity));
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
}
