//! Teaching the gateway a new OTP Pattern from a sample message.

use thiserror::Error;

use crate::app::TelegramSink;
use crate::db::{Db, DbError, MessageRow, OtpPatternRow};
use crate::events::{EventBus, MessageEvent};
use crate::otp::{derive_pattern, PatternError};
use crate::route::GENERAL_THREAD;

use super::config::SortingConfig;
use super::render::render_inbound;

/// Where the sample text for a new pattern comes from.
pub enum LearnSource {
    /// A recorded inbound message, by its numeric id.
    Message(i64),
    /// A sample body that belongs to no recorded message.
    Body(String),
}

pub struct Learned {
    pub pattern: OtpPatternRow,
    /// False when the same pattern was already stored.
    pub created: bool,
    /// The message the code was set on, for `LearnSource::Message`.
    pub message: Option<MessageRow>,
}

#[derive(Debug, Error)]
pub enum LearnError {
    #[error("Message not found")]
    NotFound,
    #[error("{0}")]
    Pattern(#[from] PatternError),
    #[error(transparent)]
    Db(#[from] DbError),
}

/// The numeric part of an inbound message id such as `in:123`.
pub fn inbound_numeric_id(message_id: &str) -> Option<i64> {
    message_id.strip_prefix("in:")?.parse().ok()
}

/// Derives a pattern from the sample and the code in it, stores it and, for a
/// recorded message, sets its code, refreshes its Telegram post and announces it.
pub async fn learn_otp(
    db: &Db,
    tg: &dyn TelegramSink,
    bus: &EventBus,
    cfg: &SortingConfig,
    source: LearnSource,
    code: &str,
    name: Option<&str>,
) -> Result<Learned, LearnError> {
    match source {
        LearnSource::Body(body) => {
            let (pattern, created) = store_pattern(db, &body, code, name)?;
            Ok(Learned {
                pattern,
                created,
                message: None,
            })
        }
        LearnSource::Message(id) => {
            let (row, tg_msg) = db.inbound_by_id(id)?.ok_or(LearnError::NotFound)?;
            let (pattern, created) = store_pattern(db, &row.body, code, name)?;
            let code = crate::otp::normalize_digits(code);
            let row = db.set_otp_code(id, &code)?.ok_or(LearnError::NotFound)?;
            if let Some(tg_msg) = tg_msg {
                refresh_post(db, tg, cfg, tg_msg, &row).await;
            }
            bus.publish(MessageEvent::updated(row.clone()));
            Ok(Learned {
                pattern,
                created,
                message: Some(row),
            })
        }
    }
}

fn store_pattern(
    db: &Db,
    body: &str,
    code: &str,
    name: Option<&str>,
) -> Result<(OtpPatternRow, bool), LearnError> {
    let pattern = derive_pattern(body, code)?;
    Ok(db.add_otp_pattern(name, &pattern)?)
}

/// Re-renders the post with the new code. A failed edit is logged: the code is
/// stored either way.
async fn refresh_post(
    db: &Db,
    tg: &dyn TelegramSink,
    cfg: &SortingConfig,
    tg_msg: i32,
    row: &MessageRow,
) {
    let number_line = match shows_number(db, row.thread_id) {
        Ok(true) => Some(row.e164.as_str()),
        Ok(false) => None,
        Err(err) => {
            tracing::warn!(error = %err, "topic lookup failed before re-render");
            return;
        }
    };
    let post = render_inbound(
        row.classification.as_ref(),
        row.otp_code.as_deref(),
        number_line,
        &row.body,
        cfg,
    );
    if let Err(err) = tg.edit_html(tg_msg, post.html).await {
        tracing::warn!(error = %err, tg_msg, "edit after teaching failed");
    }
}

/// General and Sorted Threads mix numbers, so their posts name the sender.
fn shows_number(db: &Db, thread_id: i32) -> Result<bool, DbError> {
    if thread_id == GENERAL_THREAD {
        return Ok(true);
    }
    Ok(db
        .get_topic_by_thread(thread_id)?
        .is_some_and(|t| t.sort.is_some()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::FakeTg;
    use crate::db::Topic;

    const BODY: &str = "کد ورود: 5388";

    fn cfg() -> SortingConfig {
        SortingConfig::load(None).unwrap()
    }

    fn record(db: &Db, thread_id: i32, tg_msg: Option<i32>) -> i64 {
        let row = db
            .record_inbound("/sms/1", "+98912", BODY, tg_msg, "", thread_id, None, None)
            .unwrap();
        inbound_numeric_id(&row.id).unwrap()
    }

    #[tokio::test]
    async fn learn_from_message_sets_code_edits_post_and_announces() {
        let db = Db::open_in_memory().unwrap();
        let id = record(&db, GENERAL_THREAD, Some(77));
        let tg = FakeTg::new();
        let bus = EventBus::new(8);
        let mut events = bus.subscribe();

        let learned = learn_otp(
            &db,
            &tg,
            &bus,
            &cfg(),
            LearnSource::Message(id),
            "۵۳۸۸",
            None,
        )
        .await
        .unwrap();

        assert!(learned.created);
        assert_eq!(learned.message.unwrap().otp_code.as_deref(), Some("5388"));
        let edits = tg.edits.lock().unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].0, 77);
        assert!(edits[0].1.contains("<code>5388</code>"));
        assert!(edits[0].1.contains("+98912"));
        let event = events.try_recv().expect("announced");
        assert!(event.updated);
        assert_eq!(event.otp_code.as_deref(), Some("5388"));
    }

    #[tokio::test]
    async fn contact_topic_post_has_no_number_line() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_topic(&Topic {
            thread_id: 7,
            contact_id: None,
            default_e164: Some("+98912".into()),
            title: "Ali".into(),
            ignored: false,
            sort: None,
        })
        .unwrap();
        let id = record(&db, 7, Some(80));
        let tg = FakeTg::new();

        learn_otp(
            &db,
            &tg,
            &EventBus::new(8),
            &cfg(),
            LearnSource::Message(id),
            "5388",
            None,
        )
        .await
        .unwrap();

        let edits = tg.edits.lock().unwrap();
        assert!(!edits[0].1.contains("+98912"));
    }

    #[tokio::test]
    async fn sorted_thread_post_keeps_number_line() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_sorted_topic("otp", 50, "OTP").unwrap();
        let id = record(&db, 50, Some(81));
        let tg = FakeTg::new();

        learn_otp(
            &db,
            &tg,
            &EventBus::new(8),
            &cfg(),
            LearnSource::Message(id),
            "5388",
            None,
        )
        .await
        .unwrap();

        assert!(tg.edits.lock().unwrap()[0].1.contains("+98912"));
    }

    #[tokio::test]
    async fn learn_without_known_post_still_sets_code() {
        let db = Db::open_in_memory().unwrap();
        let id = record(&db, GENERAL_THREAD, None);
        let tg = FakeTg::new();

        let learned = learn_otp(
            &db,
            &tg,
            &EventBus::new(8),
            &cfg(),
            LearnSource::Message(id),
            "5388",
            None,
        )
        .await
        .unwrap();

        assert!(tg.edits.lock().unwrap().is_empty());
        assert_eq!(learned.message.unwrap().otp_code.as_deref(), Some("5388"));
    }

    #[tokio::test]
    async fn failed_edit_does_not_fail_learning() {
        let db = Db::open_in_memory().unwrap();
        let id = record(&db, GENERAL_THREAD, Some(77));
        let tg = FakeTg::new();
        tg.fail.store(true, std::sync::atomic::Ordering::SeqCst);

        let learned = learn_otp(
            &db,
            &tg,
            &EventBus::new(8),
            &cfg(),
            LearnSource::Message(id),
            "5388",
            None,
        )
        .await
        .unwrap();

        assert_eq!(learned.message.unwrap().otp_code.as_deref(), Some("5388"));
    }

    #[tokio::test]
    async fn learn_from_body_creates_pattern_only() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let bus = EventBus::new(8);
        let mut events = bus.subscribe();

        let learned = learn_otp(
            &db,
            &tg,
            &bus,
            &cfg(),
            LearnSource::Body(BODY.into()),
            "5388",
            Some("login"),
        )
        .await
        .unwrap();

        assert!(learned.created);
        assert_eq!(learned.pattern.name, "login");
        assert!(learned.message.is_none());
        assert!(tg.edits.lock().unwrap().is_empty());
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn learn_same_pattern_twice_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        let tg = FakeTg::new();
        let bus = EventBus::new(8);
        for expect_created in [true, false] {
            let learned = learn_otp(
                &db,
                &tg,
                &bus,
                &cfg(),
                LearnSource::Body(BODY.into()),
                "5388",
                None,
            )
            .await
            .unwrap();
            assert_eq!(learned.created, expect_created);
        }
        assert_eq!(db.otp_patterns().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn learn_on_unknown_message_is_not_found() {
        let db = Db::open_in_memory().unwrap();
        let err = learn_otp(
            &db,
            &FakeTg::new(),
            &EventBus::new(8),
            &cfg(),
            LearnSource::Message(999),
            "5388",
            None,
        )
        .await;
        assert!(matches!(err, Err(LearnError::NotFound)));
    }

    #[tokio::test]
    async fn learn_rejects_code_not_in_body() {
        let db = Db::open_in_memory().unwrap();
        let id = record(&db, GENERAL_THREAD, Some(77));
        let tg = FakeTg::new();
        let err = learn_otp(
            &db,
            &tg,
            &EventBus::new(8),
            &cfg(),
            LearnSource::Message(id),
            "1111",
            None,
        )
        .await;
        assert!(matches!(
            err,
            Err(LearnError::Pattern(PatternError::CodeNotFound(_)))
        ));
        assert!(db.otp_patterns().unwrap().is_empty());
        assert!(tg.edits.lock().unwrap().is_empty());
    }

    #[test]
    fn numeric_id_parses_inbound_ids_only() {
        assert_eq!(inbound_numeric_id("in:123"), Some(123));
        assert_eq!(inbound_numeric_id("out:123"), None);
    }
}
