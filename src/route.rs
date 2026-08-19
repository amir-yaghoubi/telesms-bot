use crate::db::{Db, DbError};

pub const GENERAL_THREAD: i32 = 1;

pub enum InboundDest {
    ExistingTopic {
        thread_id: i32,
        note_switch_to: Option<String>,
    },
    CreateContactTopic {
        contact_id: i64,
        title: String,
        default_e164: String,
    },
    General {
        e164: String,
    },
}

pub fn route_inbound(db: &Db, e164: &str) -> Result<InboundDest, DbError> {
    route_to_topic(db, e164, true)
}

pub fn route_for_send(db: &Db, e164: &str) -> Result<InboundDest, DbError> {
    route_to_topic(db, e164, false)
}

fn route_to_topic(db: &Db, e164: &str, apply_default: bool) -> Result<InboundDest, DbError> {
    if db.is_ignored(e164)? {
        return Ok(InboundDest::General {
            e164: e164.to_string(),
        });
    }

    let contact = db.find_contact_by_e164(e164)?;
    let topic = match db.get_topic_by_e164(e164)? {
        Some(topic) => Some(topic),
        None => match &contact {
            Some(c) => db.get_topic_by_contact(c.id)?,
            None => None,
        },
    };

    if let Some(topic) = topic {
        let note_switch_to = if apply_default {
            apply_incoming_default(db, e164)?
        } else {
            None
        };
        return Ok(InboundDest::ExistingTopic {
            thread_id: topic.thread_id,
            note_switch_to,
        });
    }

    if let Some(contact) = contact {
        return Ok(InboundDest::CreateContactTopic {
            contact_id: contact.id,
            title: topic_title(&contact.display_name, e164),
            default_e164: e164.to_string(),
        });
    }

    Ok(InboundDest::General {
        e164: e164.to_string(),
    })
}

pub enum OutboundPlan {
    NotSms,
    Send {
        e164: String,
    },
    AskWhichNumber {
        contact_id: i64,
        numbers: Vec<String>,
    },
    UnknownTopic,
}

pub fn plan_outbound(db: &Db, thread_id: i32) -> Result<OutboundPlan, DbError> {
    if thread_id == GENERAL_THREAD {
        return Ok(OutboundPlan::NotSms);
    }
    let Some(topic) = db.get_topic_by_thread(thread_id)? else {
        return Ok(OutboundPlan::UnknownTopic);
    };
    if let Some(e164) = topic.default_e164 {
        return Ok(OutboundPlan::Send { e164 });
    }
    if let Some(contact_id) = topic.contact_id {
        let numbers = db.contact_numbers(contact_id)?;
        if numbers.len() >= 2 {
            return Ok(OutboundPlan::AskWhichNumber {
                contact_id,
                numbers,
            });
        }
        if let Some(e164) = numbers.into_iter().next() {
            db.set_default_number(thread_id, &e164)?;
            return Ok(OutboundPlan::Send { e164 });
        }
    }
    Ok(OutboundPlan::UnknownTopic)
}

pub fn apply_incoming_default(db: &Db, e164: &str) -> Result<Option<String>, DbError> {
    let Some(contact) = db.find_contact_by_e164(e164)? else {
        return Ok(None);
    };
    let Some(topic) = db.get_topic_by_contact(contact.id)? else {
        return Ok(None);
    };
    let changed = topic
        .default_e164
        .as_deref()
        .is_some_and(|current| current != e164);
    db.set_default_number(topic.thread_id, e164)?;
    if changed {
        Ok(Some(e164.to_string()))
    } else {
        Ok(None)
    }
}

pub fn topic_title(name: &str, e164: &str) -> String {
    let digits: String = e164.chars().filter(|c| c.is_ascii_digit()).collect();
    let last4 = if digits.len() >= 4 {
        &digits[digits.len() - 4..]
    } else {
        &digits
    };
    format!("{name} ({last4})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Topic;

    #[test]
    fn ignored_number_goes_to_general() {
        let db = Db::open_in_memory().unwrap();
        db.ignore_number("+989120000000").unwrap();
        let d = route_inbound(&db, "+989120000000").unwrap();
        assert!(matches!(d, InboundDest::General { .. }));
    }

    #[test]
    fn known_contact_without_topic_requests_create() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989121234567".into()])
            .unwrap();
        match route_inbound(&db, "+989121234567").unwrap() {
            InboundDest::CreateContactTopic {
                contact_id,
                title,
                default_e164,
            } => {
                assert_eq!(contact_id, id);
                assert!(title.contains("Ali"));
                assert!(title.contains("4567"));
                assert_eq!(default_e164, "+989121234567");
            }
            _ => panic!("expected CreateContactTopic"),
        }
    }

    #[test]
    fn existing_topic_and_number_switch_note() {
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
        match route_inbound(&db, b).unwrap() {
            InboundDest::ExistingTopic {
                thread_id,
                note_switch_to,
            } => {
                assert_eq!(thread_id, 42);
                assert_eq!(note_switch_to.as_deref(), Some(b));
            }
            _ => panic!("expected ExistingTopic"),
        }
    }

    #[test]
    fn general_thread_is_not_sms() {
        let db = Db::open_in_memory().unwrap();
        assert!(matches!(
            plan_outbound(&db, 1).unwrap(),
            OutboundPlan::NotSms
        ));
    }

    #[test]
    fn contact_topic_without_default_and_two_numbers_asks() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989120000001".into(), "+989120000002".into()])
            .unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: Some(id),
            default_e164: None,
            title: "Ali".into(),
            ignored: false,
        })
        .unwrap();
        match plan_outbound(&db, 42).unwrap() {
            OutboundPlan::AskWhichNumber {
                contact_id,
                numbers,
            } => {
                assert_eq!(contact_id, id);
                assert_eq!(numbers.len(), 2);
            }
            _ => panic!("expected AskWhichNumber"),
        }
    }

    #[test]
    fn contact_topic_with_default_sends() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989120000001".into()])
            .unwrap();
        db.upsert_topic(&Topic {
            thread_id: 42,
            contact_id: Some(id),
            default_e164: Some("+989120000001".into()),
            title: "Ali".into(),
            ignored: false,
        })
        .unwrap();
        match plan_outbound(&db, 42).unwrap() {
            OutboundPlan::Send { e164 } => assert_eq!(e164, "+989120000001"),
            _ => panic!("expected Send"),
        }
    }

    #[test]
    fn topic_title_uses_last_four() {
        assert_eq!(topic_title("Ali", "+989121234567"), "Ali (4567)");
    }

    #[test]
    fn route_for_send_does_not_change_default() {
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
        match route_for_send(&db, b).unwrap() {
            InboundDest::ExistingTopic {
                thread_id,
                note_switch_to,
            } => {
                assert_eq!(thread_id, 42);
                assert!(note_switch_to.is_none());
            }
            _ => panic!("expected ExistingTopic"),
        }
        assert_eq!(
            db.get_topic_by_thread(42)
                .unwrap()
                .unwrap()
                .default_e164
                .as_deref(),
            Some(a)
        );
    }
}
