use super::handlers::{
    handle_help, handle_ignore, handle_num_callback, handle_number_empty_or_list, handle_open,
    handle_pending_forward_text, handle_search, handle_sms, handle_who,
};
use super::keyboards::{
    forward_keyboard, inline_answer_articles, inline_articles, search_keyboard,
};
use super::parse::{
    allow_dm_callback, allowed, bot_commands, format_who, help_text, is_owner_dm,
    parse_cf_callback, parse_cmd_name, parse_ignore_reply, parse_num_callback, parse_open_callback,
    parse_open_cmd, parse_search_query, parse_sms_cmd, parse_status_refresh, topic_open_message,
    CfAction,
};
use super::util::{edit_failed_is_noop, forum_thread};
use crate::app::FakeTg;
use crate::config::Config;
use crate::db::{Contact, Db, Topic};
use crate::modem::FakeModem;
use crate::route::GENERAL_THREAD;
use teloxide::{ApiError, RequestError};

fn sample_cfg() -> Config {
    use std::time::Duration;

    Config {
        telegram_bot_token: "tok".into(),
        telegram_user_id: 1,
        telegram_group_id: -100,
        modem_uid: "dwm222".into(),
        google_client_id: "cid".into(),
        google_client_secret: "sec".into(),
        google_token_path: "./secrets/google-token.json".into(),
        database_path: "./data/telesms.sqlite".into(),
        contacts_sync_interval: Duration::from_secs(21600),
        default_region: "IR".into(),
        status_tz: chrono_tz::Asia::Tehran,
        sms_delete_enabled: true,
        sms_delete_max_age: Duration::from_secs(30 * 86400),
        api_key: None,
        api_bind: "0.0.0.0".into(),
        api_port: 8787,
    }
}

#[test]
fn allowed_only_owner_in_group() {
    let cfg = sample_cfg(); // helper: user 1, group -100
    assert!(allowed(&cfg, -100, 1));
    assert!(!allowed(&cfg, -100, 2));
    assert!(!allowed(&cfg, -99, 1));
    assert!(is_owner_dm(&cfg, 1, 1));
    assert!(!is_owner_dm(&cfg, -100, 1));
    assert!(!is_owner_dm(&cfg, 2, 1));
    assert!(forum_thread(GENERAL_THREAD).is_none());
    assert!(forum_thread(42).is_some());
}

#[test]
fn parse_sms_splits_number_and_rest() {
    let (n, t) = parse_sms_cmd("/sms 09121234567 hello there").unwrap();
    assert_eq!(n, "09121234567");
    assert_eq!(t, "hello there");
}

#[test]
fn parse_sms_rejects_bare() {
    assert!(parse_sms_cmd("/sms").is_none());
}

#[test]
fn bot_commands_include_forward() {
    assert!(bot_commands().iter().any(|c| c.command == "forward"));
}

#[test]
fn parse_cf_callbacks() {
    assert_eq!(parse_cf_callback("cf:off"), Some(CfAction::Disable));
    assert_eq!(parse_cf_callback("cf:cancel"), Some(CfAction::Cancel));
    assert_eq!(parse_cf_callback("cf:type"), Some(CfAction::TypeNumber));
    assert_eq!(parse_cf_callback("cf:search"), Some(CfAction::Search));
    assert_eq!(parse_cf_callback("cf:c:42"), Some(CfAction::Contact(42)));
    assert_eq!(
        parse_cf_callback("cf:n:+989121234567"),
        Some(CfAction::Number("+989121234567".into()))
    );
}

#[test]
fn help_mentions_forward() {
    assert!(help_text().contains("/forward"));
}

#[test]
fn forward_keyboard_has_four_actions() {
    let keyboard = forward_keyboard();
    assert_eq!(keyboard.inline_keyboard.len(), 2);
    assert_eq!(
        keyboard.inline_keyboard.iter().map(Vec::len).sum::<usize>(),
        4
    );
}

#[tokio::test]
async fn pending_forward_number_consumes_text_and_clears_pending() {
    use crate::db::PendingForwardMode;

    let db = Db::open_in_memory().unwrap();
    db.set_pending_forward(9, PendingForwardMode::Number, -100, 77)
        .unwrap();
    let modem = FakeModem::default();

    let result = handle_pending_forward_text(&db, "IR", 9, "09121234567", &modem)
        .await
        .unwrap()
        .expect("pending input handled");

    assert_eq!(result.pending.edit_message_id, 77);
    assert!(result.state.unwrap().enabled);
    assert_eq!(
        modem.forward.lock().unwrap().e164.as_deref(),
        Some("+989121234567")
    );
    assert!(db.get_pending_forward(9).unwrap().is_none());
}

#[tokio::test]
async fn pending_forward_does_not_consume_commands() {
    use crate::db::PendingForwardMode;

    let db = Db::open_in_memory().unwrap();
    db.set_pending_forward(9, PendingForwardMode::Number, -100, 77)
        .unwrap();
    let modem = FakeModem::default();

    let result = handle_pending_forward_text(&db, "IR", 9, "/sms 0912 hello", &modem)
        .await
        .unwrap();

    assert!(result.is_none());
    assert!(db.get_pending_forward(9).unwrap().is_none());
}

#[test]
fn format_who_marks_default() {
    let t = Topic {
        thread_id: 9,
        contact_id: Some(1),
        default_e164: Some("+98912".into()),
        title: "Ali".into(),
        ignored: false,
    };
    let s = format_who(&t, Some("Ali"), &["+98912".into(), "+98913".into()]);
    assert!(s.contains("Ali"));
    assert!(s.contains("+98912"));
    assert!(s.contains("default") || s.contains("*"));
}

#[tokio::test]
async fn sms_known_contact_creates_topic_sends_and_acks() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_contact("people/a", "Ali").unwrap();
    db.replace_contact_numbers(id, &["+989121234567".into()])
        .unwrap();
    let tg = FakeTg::new();
    let modem = FakeModem::default();
    handle_sms(
        &db,
        "IR",
        "09121234567",
        "hello",
        1,
        Some(7),
        &modem,
        &tg,
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        modem.sent.lock().unwrap().as_slice(),
        &[("+989121234567".into(), "hello".into())]
    );
    assert_eq!(
        tg.reactions.lock().unwrap().as_slice(),
        &[
            (7, crate::app::SEND_PENDING.into()),
            (7, crate::app::SEND_REACT_OK.into()),
        ]
    );
    assert!(tg.replies.lock().unwrap().is_empty());
    assert!(db.get_topic_by_contact(id).unwrap().is_some());
}

#[tokio::test]
async fn who_in_general_says_so() {
    let db = Db::open_in_memory().unwrap();
    let tg = FakeTg::new();
    handle_who(&db, "IR", 1, &tg).await.unwrap();
    assert_eq!(
        tg.posts.lock().unwrap().as_slice(),
        &[(1, "this is General".into())]
    );
}

#[tokio::test]
async fn who_in_contact_topic_lists_numbers() {
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
    let tg = FakeTg::new();
    handle_who(&db, "IR", 9, &tg).await.unwrap();
    let posts = tg.posts.lock().unwrap();
    assert!(posts[0].1.contains("Ali"));
    assert!(posts[0].1.contains("+98912 (default)"));
    assert!(posts[0].1.contains("+98913"));
}

#[tokio::test]
async fn number_empty_posts_no_numbers() {
    let db = Db::open_in_memory().unwrap();
    let tg = FakeTg::new();
    let numbers = handle_number_empty_or_list(&db, "IR", 1, &tg)
        .await
        .unwrap();
    assert!(numbers.is_empty());
    assert_eq!(
        tg.posts.lock().unwrap().as_slice(),
        &[(1, "no numbers".into())]
    );
}

#[tokio::test]
async fn num_callback_sets_default_and_posts() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_topic(&Topic {
        thread_id: 9,
        contact_id: None,
        default_e164: Some("+98912".into()),
        title: "Ali".into(),
        ignored: false,
    })
    .unwrap();
    let tg = FakeTg::new();
    handle_num_callback(&db, "IR", 9, "+98912", None, &tg, true)
        .await
        .unwrap();
    assert_eq!(
        db.get_topic_by_thread(9)
            .unwrap()
            .unwrap()
            .default_e164
            .as_deref(),
        Some("+98912")
    );
    assert_eq!(
        tg.posts.lock().unwrap().as_slice(),
        &[(9, "default is +98912".into())]
    );
}

#[tokio::test]
async fn num_callback_sends_pending_text() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_contact("people/a", "Ali").unwrap();
    db.replace_contact_numbers(id, &["+989188086139".into(), "+989025438263".into()])
        .unwrap();
    db.upsert_topic(&Topic {
        thread_id: 9,
        contact_id: Some(id),
        default_e164: None,
        title: "Ali".into(),
        ignored: false,
    })
    .unwrap();
    db.set_pending_outbound(9, "hello", Some(11)).unwrap();
    let tg = FakeTg::new();
    let modem = FakeModem::default();
    handle_num_callback(&db, "IR", 9, "+989188086139", Some(&modem), &tg, true)
        .await
        .unwrap();
    assert_eq!(
        modem.sent.lock().unwrap().as_slice(),
        &[("+989188086139".into(), "hello".into())]
    );
    assert_eq!(
        tg.reactions.lock().unwrap().as_slice(),
        &[
            (11, crate::app::SEND_PENDING.into()),
            (11, crate::app::SEND_REACT_OK.into()),
        ]
    );
    assert!(tg.replies.lock().unwrap().is_empty());
    assert!(db.take_pending_outbound(9).unwrap().is_none());
}

#[test]
fn parse_num_callback_reads_e164() {
    assert_eq!(parse_num_callback("num:+98912"), Some("+98912"));
    assert!(parse_num_callback("other:+98912").is_none());
    assert!(parse_num_callback("num:").is_none());
}

#[tokio::test]
async fn ignore_contact_topic_ignores_all_numbers() {
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
    let tg = FakeTg::new();
    handle_ignore(&db, "IR", 9, None, &tg).await.unwrap();
    assert!(db.is_ignored("+98912").unwrap());
    assert!(db.is_ignored("+98913").unwrap());
    assert!(tg.posts.lock().unwrap()[0].1.contains("ignored"));
}

#[tokio::test]
async fn ignore_general_without_reply_posts_hint() {
    let db = Db::open_in_memory().unwrap();
    let tg = FakeTg::new();
    handle_ignore(&db, "IR", 1, None, &tg).await.unwrap();
    assert_eq!(
        tg.posts.lock().unwrap().as_slice(),
        &[(1, "reply to a +number to ignore it".into())]
    );
}

#[tokio::test]
async fn ignore_contact_topic_empty_numbers_posts_hint() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_contact("people/a", "Ali").unwrap();
    db.upsert_topic(&Topic {
        thread_id: 9,
        contact_id: Some(id),
        default_e164: None,
        title: "Ali".into(),
        ignored: false,
    })
    .unwrap();
    let tg = FakeTg::new();
    handle_ignore(&db, "IR", 9, None, &tg).await.unwrap();
    assert_eq!(
        tg.posts.lock().unwrap().as_slice(),
        &[(9, "reply to a +number to ignore it".into())]
    );
}

#[tokio::test]
async fn num_callback_avoids_identity_conflict() {
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
    db.upsert_topic(&Topic {
        thread_id: 10,
        contact_id: None,
        default_e164: Some("+98913".into()),
        title: "+98913".into(),
        ignored: false,
    })
    .unwrap();
    let tg = FakeTg::new();
    handle_num_callback(&db, "IR", 9, "+98913", None, &tg, true)
        .await
        .unwrap();
    assert_eq!(
        db.get_topic_by_thread(9)
            .unwrap()
            .unwrap()
            .default_e164
            .as_deref(),
        Some("+98913")
    );
    assert_eq!(
        tg.posts.lock().unwrap().as_slice(),
        &[(9, "default is +98913".into())]
    );
}

#[tokio::test]
async fn ignore_general_reply_parses_plus() {
    let db = Db::open_in_memory().unwrap();
    let tg = FakeTg::new();
    handle_ignore(&db, "IR", 1, Some("+989121234567\nhi"), &tg)
        .await
        .unwrap();
    assert!(db.is_ignored("+989121234567").unwrap());
}

#[test]
fn parse_ignore_reply_requires_plus() {
    assert_eq!(parse_ignore_reply("+98912\nhello"), Some("+98912"));
    assert!(parse_ignore_reply("hello\n+98912").is_none());
}

#[tokio::test]
async fn sms_ignored_number_stays_in_general() {
    let db = Db::open_in_memory().unwrap();
    db.ignore_number("+989121234567").unwrap();
    let tg = FakeTg::new();
    let modem = FakeModem::default();
    handle_sms(
        &db,
        "IR",
        "09121234567",
        "hello",
        1,
        Some(7),
        &modem,
        &tg,
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        modem.sent.lock().unwrap().as_slice(),
        &[("+989121234567".into(), "hello".into())]
    );
    assert_eq!(
        tg.reactions.lock().unwrap().as_slice(),
        &[
            (7, crate::app::SEND_PENDING.into()),
            (7, crate::app::SEND_REACT_OK.into()),
        ]
    );
    assert!(tg.replies.lock().unwrap().is_empty());
    assert!(db.get_topic_by_e164("+989121234567").unwrap().is_none());
}

#[tokio::test]
async fn sms_does_not_apply_incoming_default() {
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
    let modem = FakeModem::default();
    handle_sms(&db, "IR", b, "hello", 1, Some(7), &modem, &tg, true)
        .await
        .unwrap();
    assert_eq!(
        db.get_topic_by_thread(42)
            .unwrap()
            .unwrap()
            .default_e164
            .as_deref(),
        Some(a)
    );
    assert_eq!(
        modem.sent.lock().unwrap().as_slice(),
        &[(b.into(), "hello".into())]
    );
    assert_eq!(
        tg.reactions.lock().unwrap().as_slice(),
        &[
            (7, crate::app::SEND_PENDING.into()),
            (7, crate::app::SEND_REACT_OK.into()),
        ]
    );
    assert!(tg.replies.lock().unwrap().is_empty());
}

#[tokio::test]
async fn sms_send_ok_deletes_path() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_contact("people/a", "Ali").unwrap();
    db.replace_contact_numbers(id, &["+989121234567".into()])
        .unwrap();
    let tg = FakeTg::new();
    let modem = FakeModem::default();
    handle_sms(
        &db,
        "IR",
        "09121234567",
        "hello",
        1,
        None,
        &modem,
        &tg,
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        modem.deleted.lock().unwrap().as_slice(),
        &["/fake/sms/1".into()] as &[String]
    );
}

#[tokio::test]
async fn sms_send_err_does_not_delete() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_contact("people/a", "Ali").unwrap();
    db.replace_contact_numbers(id, &["+989121234567".into()])
        .unwrap();
    let tg = FakeTg::new();
    let modem = FakeModem {
        fail: true,
        ..FakeModem::default()
    };
    handle_sms(
        &db,
        "IR",
        "09121234567",
        "hello",
        1,
        None,
        &modem,
        &tg,
        true,
    )
    .await
    .unwrap();
    assert!(modem.deleted.lock().unwrap().is_empty());
}

#[test]
fn parse_search_query_reads_rest() {
    assert_eq!(parse_search_query("/search ali"), Some("ali"));
    assert_eq!(parse_search_query("/search@bot Ali Reza"), Some("Ali Reza"));
    assert!(parse_search_query("/search").is_none());
    assert!(parse_search_query("/searching ali").is_none());
}

#[test]
fn parse_open_callback_reads_id() {
    assert_eq!(parse_open_callback("open:42"), Some(42));
    assert!(parse_open_callback("open:").is_none());
    assert!(parse_open_callback("num:1").is_none());
}

#[test]
fn parse_open_cmd_reads_id() {
    assert_eq!(parse_open_cmd("/open 42"), Some(42));
    assert_eq!(parse_open_cmd("/open@bot 7"), Some(7));
    assert!(parse_open_cmd("/open").is_none());
    assert!(parse_open_cmd("/opening 1").is_none());
}

#[test]
fn topic_open_message_link_or_fallback() {
    let t = Topic {
        thread_id: 9,
        contact_id: Some(1),
        default_e164: None,
        title: "Ali".into(),
        ignored: false,
    };
    assert_eq!(
        topic_open_message(-1001234567890, &t),
        "open topic\nhttps://t.me/c/1234567890/9"
    );
    assert_eq!(topic_open_message(-100, &t), "topic exists: Ali");
}

#[tokio::test]
async fn search_empty_query_posts_usage() {
    let db = Db::open_in_memory().unwrap();
    let tg = FakeTg::new();
    let hits = handle_search(&db, 1, "", &tg).await.unwrap();
    assert!(hits.is_empty());
    assert_eq!(
        tg.posts.lock().unwrap().as_slice(),
        &[(1, "usage: /search <query>".into())]
    );
}

#[tokio::test]
async fn search_no_matches_posts() {
    let db = Db::open_in_memory().unwrap();
    let tg = FakeTg::new();
    let hits = handle_search(&db, 1, "ali", &tg).await.unwrap();
    assert!(hits.is_empty());
    assert_eq!(
        tg.posts.lock().unwrap().as_slice(),
        &[(1, "no matches".into())]
    );
}

#[tokio::test]
async fn search_returns_cached_contacts() {
    let db = Db::open_in_memory().unwrap();
    db.upsert_contact("people/x", "Ali").unwrap();
    let tg = FakeTg::new();
    let hits = handle_search(&db, 1, "ali", &tg).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].display_name, "Ali");
    assert!(tg.posts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn search_unavailable_when_contacts_flag_cleared() {
    let db = Db::open_in_memory().unwrap();
    db.set_contacts_available(false);
    let tg = FakeTg::new();
    let hits = handle_search(&db, 1, "ali", &tg).await.unwrap();
    assert!(hits.is_empty());
    assert_eq!(
        tg.posts.lock().unwrap().as_slice(),
        &[(1, "contacts unavailable".into())]
    );
}

#[tokio::test]
async fn open_creates_topic_with_single_number_default() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_contact("people/x", "Ali").unwrap();
    db.replace_contact_numbers(id, &["+98912".into()]).unwrap();
    let tg = FakeTg::new();
    handle_open(&db, -1001234567890, 1, id, &tg).await.unwrap();
    let topic = db.get_topic_by_contact(id).unwrap().unwrap();
    assert_eq!(topic.default_e164.as_deref(), Some("+98912"));
    assert_eq!(topic.thread_id, 100);
    assert!(tg.posts.lock().unwrap()[0]
        .1
        .contains("https://t.me/c/1234567890/100"));
}

#[tokio::test]
async fn open_multi_number_has_no_default() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_contact("people/x", "Ali").unwrap();
    db.replace_contact_numbers(id, &["+98912".into(), "+98913".into()])
        .unwrap();
    let tg = FakeTg::new();
    handle_open(&db, -1001234567890, 1, id, &tg).await.unwrap();
    let topic = db.get_topic_by_contact(id).unwrap().unwrap();
    assert!(topic.default_e164.is_none());
}

#[tokio::test]
async fn open_existing_posts_link_without_create() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_contact("people/x", "Ali").unwrap();
    db.upsert_topic(&Topic {
        thread_id: 9,
        contact_id: Some(id),
        default_e164: Some("+98912".into()),
        title: "Ali".into(),
        ignored: false,
    })
    .unwrap();
    let tg = FakeTg::new();
    handle_open(&db, -1001234567890, 1, id, &tg).await.unwrap();
    assert_eq!(
        tg.posts.lock().unwrap().as_slice(),
        &[(1, "open topic\nhttps://t.me/c/1234567890/9".into())]
    );
    assert_eq!(db.get_topic_by_contact(id).unwrap().unwrap().thread_id, 9);
}

#[test]
fn inline_articles_cap_20() {
    let hits: Vec<Contact> = (0..25)
        .map(|i| Contact {
            id: i,
            google_resource: format!("p/{i}"),
            display_name: format!("N{i}"),
            numbers: vec![],
            ambiguous: false,
        })
        .collect();
    assert_eq!(inline_articles(&hits).len(), 20);
}

#[test]
fn search_keyboard_caps_20() {
    let hits: Vec<Contact> = (0..25)
        .map(|i| Contact {
            id: i,
            google_resource: format!("p/{i}"),
            display_name: format!("N{i}"),
            numbers: vec![],
            ambiguous: false,
        })
        .collect();
    assert_eq!(search_keyboard(&hits).inline_keyboard.len(), 20);
}

#[test]
fn inline_articles_id_title_first_number() {
    let hits = [Contact {
        id: 9,
        google_resource: "p/9".into(),
        display_name: "Ali".into(),
        numbers: vec!["+98912".into(), "+98913".into()],
        ambiguous: false,
    }];
    assert_eq!(
        inline_articles(&hits),
        vec![("9".into(), "Ali".into(), "+98912".into())]
    );
}

fn sample_hit() -> Contact {
    Contact {
        id: 9,
        google_resource: "p/9".into(),
        display_name: "Ali".into(),
        numbers: vec!["+98912".into()],
        ambiguous: false,
    }
}

#[test]
fn inline_answer_empty_query_no_results() {
    let hits = [sample_hit()];
    assert!(inline_answer_articles("", Ok(&hits)).is_empty());
    assert!(inline_answer_articles("  ", Ok(&hits)).is_empty());
}

#[test]
fn inline_answer_search_error_contacts_unavailable() {
    let arts = inline_answer_articles("ali", Err(()));
    assert_eq!(arts.len(), 1);
    assert_eq!(arts[0].0, "unavailable");
    assert_eq!(arts[0].1, "contacts unavailable");
}

#[test]
fn parse_status_cmd_and_refresh() {
    assert_eq!(parse_cmd_name("/status"), Some("status"));
    assert_eq!(parse_cmd_name("/status@bot"), Some("status"));
    assert_eq!(parse_cmd_name("/status extra"), Some("status"));
    assert!(parse_status_refresh("st:r"));
    assert!(!parse_status_refresh("st:x"));
    assert!(!parse_status_refresh("num:+1"));
}

#[test]
fn dm_callback_is_status_only() {
    assert!(allow_dm_callback("st:r"));
    assert!(!allow_dm_callback("num:+1"));
    assert!(!allow_dm_callback("open:1"));
    assert!(!allow_dm_callback("st:x"));
}

#[test]
fn identical_refresh_edit_is_noop() {
    assert!(edit_failed_is_noop(&RequestError::Api(
        ApiError::MessageNotModified
    )));
    assert!(!edit_failed_is_noop(&RequestError::Api(
        ApiError::MessageToEditNotFound
    )));
    assert!(!edit_failed_is_noop(&RequestError::Api(
        ApiError::MessageCantBeEdited
    )));
}

#[test]
fn bot_commands_are_the_forum_menu() {
    let cmds = bot_commands();
    let names: Vec<&str> = cmds.iter().map(|c| c.command.as_str()).collect();
    assert_eq!(
        names.as_slice(),
        &["help", "sms", "search", "who", "number", "ignore", "forward", "status"]
    );
    assert_eq!(cmds[0].description, "How to send SMS and use this group");
    assert_eq!(cmds[1].description, "Send SMS: /sms <number> <text>");
    assert_eq!(cmds[2].description, "Find a contact and open their topic");
    assert_eq!(cmds[3].description, "Show this topic's contact and numbers");
    assert_eq!(
        cmds[4].description,
        "Choose the default number for this topic"
    );
    assert_eq!(
        cmds[5].description,
        "Stop auto-creating a topic for this number"
    );
    assert_eq!(cmds[6].description, "Manage unconditional call forwarding");
    assert_eq!(cmds[7].description, "Modem and gateway status");
}

#[test]
fn help_text_matches_spec_and_covers_catalog() {
    let text = help_text();
    assert_eq!(
        text,
        "\
SMS from this forum.

/help
  This message.

/sms <number> <text>
  Send an SMS from any topic. Creates or opens the contact topic.

/search <query>
  Find a Google contact. Tap to open or create their topic.

/who
  Contact topic: name, numbers, current default.
  General: says this is General.

/number
  Contact topic: buttons to set the default number.

/ignore
  Contact topic: stop auto-creating a topic for these numbers.
  General: reply to a +number message to ignore it.

/forward
  Show or change unconditional call forwarding.

/status
  Modem, SIM, today's SMS counts, last in/out, contacts.
  Works in any topic and in a private chat with the bot.

Typing in a contact topic sends SMS to that contact's default number.
Text in General is not an SMS unless you use /sms."
    );
    for cmd in bot_commands() {
        assert!(
            text.contains(&cmd.command),
            "help_text missing catalog command {}",
            cmd.command
        );
    }
    assert!(text.contains("/sms <number>"));
    assert!(text.contains("contact topic"));
    assert!(text.contains("Text in General is not an SMS unless you use /sms."));
}

#[test]
fn parse_cmd_name_help() {
    assert_eq!(parse_cmd_name("/help"), Some("help"));
    assert_eq!(parse_cmd_name("/help@bot"), Some("help"));
    assert_eq!(parse_cmd_name("/help extra"), Some("help"));
}

#[tokio::test]
async fn help_posts_guide_in_same_thread() {
    let tg = FakeTg::new();
    handle_help(9, &tg).await.unwrap();
    assert_eq!(
        tg.posts.lock().unwrap().as_slice(),
        &[(9, help_text().to_string())]
    );
}
