use std::sync::Arc;

use teloxide::dispatching::dialogue::GetChatId;
use teloxide::dispatching::{Dispatcher, UpdateHandler};
use teloxide::error_handlers::LoggingErrorHandler;
use teloxide::prelude::*;
use teloxide::types::{BotCommandScope, CallbackQuery, ChatId, InlineQuery, MessageId, ParseMode};

use crate::actions::{search_contacts, who, ActionError, Identity};
use crate::app::{handle_owner_text, send_and_ack, AppError, OwnerTextOutcome, TelegramSink};
use crate::config::Config;
use crate::db::{Contact, Db, Topic};
use crate::modem::SmsModem;
use crate::normalize::normalize_e164;
use crate::route::{route_for_send, topic_title, InboundDest, GENERAL_THREAD};

use super::keyboards::{
    inline_answer_articles, inline_query_results, number_keyboard, search_keyboard, status_keyboard,
};
use super::parse::{
    allow_dm_callback, allowed, bot_commands, format_who, help_text, is_owner_dm, parse_cmd_name,
    parse_ignore_reply, parse_num_callback, parse_open_callback, parse_open_cmd,
    parse_search_query, parse_sms_cmd, parse_status_refresh, topic_open_message,
};
use super::sink::RealTg;
use super::util::{edit_failed_is_noop, forum_thread, thread_id_i32};

pub(crate) async fn handle_help(thread_id: i32, tg: &dyn TelegramSink) -> Result<(), AppError> {
    tg.post(thread_id, help_text().to_string()).await
}

async fn register_commands(bot: &Bot, group_id: i64) {
    let result = bot
        .set_my_commands(bot_commands())
        .scope(BotCommandScope::Chat {
            chat_id: ChatId(group_id).into(),
        })
        .await;
    if let Err(err) = result {
        tracing::warn!(error = %err, "setMyCommands failed");
    }
}

fn topic_numbers(db: &Db, topic: &Topic) -> Result<Vec<String>, AppError> {
    if let Some(contact_id) = topic.contact_id {
        Ok(db.contact_numbers(contact_id)?)
    } else {
        Ok(topic.default_e164.clone().into_iter().collect())
    }
}

pub(crate) async fn handle_who(
    db: &Db,
    region: &str,
    thread_id: i32,
    tg: &dyn TelegramSink,
) -> Result<(), AppError> {
    if thread_id == GENERAL_THREAD {
        tg.post(thread_id, "this is General".to_string()).await?;
        return Ok(());
    }
    let Some(topic) = db.get_topic_by_thread(thread_id)? else {
        tg.post(thread_id, "unknown topic".to_string()).await?;
        return Ok(());
    };
    if topic.contact_id.is_none() && topic.default_e164.is_none() {
        tg.post(thread_id, "unknown topic".to_string()).await?;
        return Ok(());
    }
    let identity = Identity {
        contact_id: topic.contact_id,
        number: topic.default_e164.clone(),
        thread_id: Some(thread_id),
    };
    match who(db, region, &identity) {
        Ok(w) => {
            let topic = Topic {
                thread_id: w.thread_id,
                contact_id: w.contact_id,
                default_e164: w.default_e164.clone(),
                title: w.display_name.clone(),
                ignored: false,
            };
            let mut text = format_who(&topic, Some(&w.display_name), &w.numbers);
            if w.ambiguous {
                text.push_str("\n(also on another contact)");
            }
            tg.post(thread_id, text).await?;
            Ok(())
        }
        Err(ActionError::NotFound(_)) => {
            tg.post(thread_id, "unknown topic".to_string()).await?;
            Ok(())
        }
        Err(ActionError::Db(e)) => Err(e.into()),
        Err(e) => Err(AppError::Telegram(e.to_string())),
    }
}

pub(crate) async fn handle_number_empty_or_list(
    db: &Db,
    thread_id: i32,
    tg: &dyn TelegramSink,
) -> Result<Vec<String>, AppError> {
    let numbers = match db.get_topic_by_thread(thread_id)? {
        Some(topic) => topic_numbers(db, &topic)?,
        None => Vec::new(),
    };
    if numbers.is_empty() {
        tg.post(thread_id, "no numbers".to_string()).await?;
    }
    Ok(numbers)
}

pub(crate) async fn handle_num_callback(
    db: &Db,
    region: &str,
    thread_id: i32,
    e164: &str,
    modem: Option<&dyn SmsModem>,
    tg: &dyn TelegramSink,
    delete_enabled: bool,
) -> Result<(), AppError> {
    db.set_default_number(thread_id, e164)?;
    tg.post(thread_id, format!("default is {e164}")).await?;
    if let (Some(modem), Some((pending, pending_reply))) =
        (modem, db.take_pending_outbound(thread_id)?)
    {
        handle_owner_text(
            db,
            region,
            thread_id,
            &pending,
            pending_reply,
            modem,
            tg,
            delete_enabled,
        )
        .await?;
    }
    Ok(())
}

async fn send_number_buttons(
    bot: &Bot,
    chat_id: ChatId,
    thread_id: i32,
    numbers: &[String],
) -> Result<(), AppError> {
    let mut req = bot
        .send_message(chat_id, "which number?")
        .reply_markup(number_keyboard(numbers));
    if let Some(thread) = forum_thread(thread_id) {
        req = req.message_thread_id(thread);
    }
    req.await.map_err(|e| AppError::Telegram(e.to_string()))?;
    Ok(())
}

pub(crate) async fn handle_search(
    db: &Db,
    thread_id: i32,
    query: &str,
    tg: &dyn TelegramSink,
) -> Result<Vec<Contact>, AppError> {
    if query.is_empty() {
        tg.post(thread_id, "usage: /search <query>".to_string())
            .await?;
        return Ok(Vec::new());
    }
    match search_contacts(db, query) {
        Ok(hits) if hits.is_empty() => {
            tg.post(thread_id, "no matches".to_string()).await?;
            Ok(hits)
        }
        Ok(hits) => Ok(hits),
        Err(ActionError::Validation(_)) => {
            tg.post(thread_id, "usage: /search <query>".to_string())
                .await?;
            Ok(Vec::new())
        }
        Err(ActionError::ContactsUnavailable) => {
            tg.post(thread_id, "contacts unavailable".to_string())
                .await?;
            Ok(Vec::new())
        }
        Err(ActionError::Db(e)) => Err(e.into()),
        Err(e) => Err(AppError::Telegram(e.to_string())),
    }
}

pub(crate) async fn handle_open(
    db: &Db,
    group_id: i64,
    reply_thread: i32,
    contact_id: i64,
    tg: &dyn TelegramSink,
) -> Result<(), AppError> {
    if let Some(topic) = db.get_topic_by_contact(contact_id)? {
        tg.post(reply_thread, topic_open_message(group_id, &topic))
            .await?;
        return Ok(());
    }
    let Some(contact) = db.get_contact(contact_id)? else {
        tg.post(reply_thread, "unknown contact".to_string()).await?;
        return Ok(());
    };
    let default_e164 = if contact.numbers.len() == 1 {
        contact.numbers.first().cloned()
    } else {
        None
    };
    let title = match contact.numbers.first() {
        Some(n) => topic_title(&contact.display_name, n),
        None => contact.display_name.clone(),
    };
    let topic = Topic {
        thread_id: 0,
        contact_id: Some(contact.id),
        default_e164,
        title: title.clone(),
        ignored: false,
    };
    let thread_id = tg.create_topic(title).await?;
    let topic = Topic { thread_id, ..topic };
    db.upsert_topic(&topic)?;
    tg.post(reply_thread, topic_open_message(group_id, &topic))
        .await?;
    Ok(())
}

pub(crate) async fn handle_ignore(
    db: &Db,
    region: &str,
    thread_id: i32,
    reply_text: Option<&str>,
    tg: &dyn TelegramSink,
) -> Result<(), AppError> {
    let mut targets = Vec::new();
    if thread_id != GENERAL_THREAD {
        if let Some(topic) = db.get_topic_by_thread(thread_id)? {
            targets = topic_numbers(db, &topic)?;
            if targets.is_empty() {
                if let Some(e164) = topic.default_e164 {
                    targets.push(e164);
                }
            }
        }
    } else if let Some(raw) = reply_text.and_then(parse_ignore_reply) {
        match normalize_e164(raw, region) {
            Ok(e164) => targets.push(e164),
            Err(_) => targets.push(raw.to_string()),
        }
    }

    if targets.is_empty() {
        tg.post(thread_id, "reply to a +number to ignore it".to_string())
            .await?;
        return Ok(());
    }

    for n in &targets {
        db.ignore_number(n)?;
    }
    tg.post(thread_id, format!("ignored {}", targets.join(", ")))
        .await?;
    Ok(())
}

pub async fn handle_sms(
    db: &Db,
    region: &str,
    raw_number: &str,
    text: &str,
    reply_thread: i32,
    reply_to: Option<i32>,
    modem: &dyn SmsModem,
    tg: &dyn TelegramSink,
    delete_enabled: bool,
) -> Result<(), AppError> {
    let e164 = match normalize_e164(raw_number, region) {
        Ok(e164) => e164,
        Err(err) => {
            tg.post(reply_thread, err.to_string()).await?;
            return Ok(());
        }
    };

    let thread_id = match route_for_send(db, &e164)? {
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
    };

    send_and_ack(
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
    Ok(())
}

async fn post_status(
    bot: &Bot,
    chat_id: ChatId,
    thread_id: i32,
    db: &Db,
    info: &dyn crate::modem::ModemInfo,
    cfg: &Config,
    edit: Option<MessageId>,
) -> Result<(), AppError> {
    let html =
        match crate::status::gather(info, db, cfg.status_tz, &cfg.modem_uid, chrono::Utc::now())
            .await
        {
            Ok(snap) => crate::status::format_status_html(&snap),
            Err(err) => {
                let text = format!("status failed: {err}");
                if let Some(id) = edit {
                    let _ = bot.edit_message_text(chat_id, id, &text).await;
                } else {
                    let mut req = bot.send_message(chat_id, text);
                    if let Some(thread) = forum_thread(thread_id) {
                        req = req.message_thread_id(thread);
                    }
                    req.await.map_err(|e| AppError::Telegram(e.to_string()))?;
                }
                return Ok(());
            }
        };
    if let Some(id) = edit {
        let req = bot
            .edit_message_text(chat_id, id, html.clone())
            .parse_mode(ParseMode::Html)
            .reply_markup(status_keyboard());
        match req.await {
            Ok(_) => {}
            Err(err) if edit_failed_is_noop(&err) => {}
            Err(err) => {
                tracing::warn!(error = %err, "status card edit failed; sending new");
                let mut send = bot
                    .send_message(chat_id, html)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(status_keyboard());
                if let Some(thread) = forum_thread(thread_id) {
                    send = send.message_thread_id(thread);
                }
                send.await.map_err(|e| AppError::Telegram(e.to_string()))?;
            }
        }
        return Ok(());
    }
    let mut req = bot
        .send_message(chat_id, html)
        .parse_mode(ParseMode::Html)
        .reply_markup(status_keyboard());
    if let Some(thread) = forum_thread(thread_id) {
        req = req.message_thread_id(thread);
    }
    req.await.map_err(|e| AppError::Telegram(e.to_string()))?;
    Ok(())
}

pub fn schema() -> UpdateHandler<AppError> {
    dptree::entry()
        .branch(
            Update::filter_message()
                .filter(|cfg: Config, msg: Message| {
                    msg.from
                        .as_ref()
                        .is_some_and(|u| is_owner_dm(&cfg, msg.chat.id.0, u.id.0 as i64))
                })
                .endpoint(on_owner_dm),
        )
        .branch(
            Update::filter_message()
                .filter(|cfg: Config, msg: Message| {
                    msg.from
                        .as_ref()
                        .is_some_and(|u| allowed(&cfg, msg.chat.id.0, u.id.0 as i64))
                })
                .endpoint(on_message),
        )
        .branch(
            Update::filter_callback_query()
                .filter(|cfg: Config, q: CallbackQuery| {
                    q.chat_id().is_some_and(|ChatId(id)| {
                        allowed(&cfg, id, q.from.id.0 as i64)
                            || is_owner_dm(&cfg, id, q.from.id.0 as i64)
                    })
                })
                .endpoint(on_callback),
        )
        .branch(
            Update::filter_inline_query()
                .filter(|cfg: Config, q: InlineQuery| q.from.id.0 as i64 == cfg.telegram_user_id)
                .endpoint(on_inline_query),
        )
}

async fn on_owner_dm(
    bot: Bot,
    msg: Message,
    cfg: Config,
    db: Arc<Db>,
    info: Arc<dyn crate::modem::ModemInfo>,
) -> Result<(), AppError> {
    if parse_cmd_name(msg.text().unwrap_or("")) == Some("status") {
        return post_status(
            &bot,
            msg.chat.id,
            crate::route::GENERAL_THREAD,
            &db,
            info.as_ref(),
            &cfg,
            None,
        )
        .await;
    }
    bot.send_message(
        msg.chat.id,
        format!(
            "Commands only work in the forum group (id {}).\n\n\
Open that group and send them there, for example:\n\
/sms 0912xxxxxxx hello\n\
/search ali\n\n\
The bot must be a group admin with Manage Topics.",
            cfg.telegram_group_id
        ),
    )
    .await
    .map_err(|e| AppError::Telegram(e.to_string()))?;
    Ok(())
}

async fn on_message(
    bot: Bot,
    msg: Message,
    cfg: Config,
    db: Arc<Db>,
    modem: Arc<dyn SmsModem>,
    info: Arc<dyn crate::modem::ModemInfo>,
) -> Result<(), AppError> {
    let Some(text) = msg.text() else {
        return Ok(());
    };
    let thread_id = thread_id_i32(&msg);
    let MessageId(reply_to) = msg.id;
    let tg = RealTg {
        bot: bot.clone(),
        chat_id: ChatId(cfg.telegram_group_id),
    };
    if let Some((raw, body)) = parse_sms_cmd(text) {
        handle_sms(
            &db,
            &cfg.default_region,
            &raw,
            &body,
            thread_id,
            Some(reply_to),
            modem.as_ref(),
            &tg,
            cfg.sms_delete_enabled,
        )
        .await?;
        return Ok(());
    }
    match parse_cmd_name(text) {
        Some("help") => handle_help(thread_id, &tg).await?,
        Some("who") => handle_who(&db, &cfg.default_region, thread_id, &tg).await?,
        Some("number") => {
            let numbers = handle_number_empty_or_list(&db, thread_id, &tg).await?;
            if !numbers.is_empty() {
                send_number_buttons(&bot, tg.chat_id, thread_id, &numbers).await?;
            }
        }
        Some("ignore") => {
            let reply = msg.reply_to_message().and_then(|m| m.text());
            handle_ignore(&db, &cfg.default_region, thread_id, reply, &tg).await?;
        }
        Some("search") => {
            let q = parse_search_query(text).unwrap_or("");
            let hits = handle_search(&db, thread_id, q, &tg).await?;
            if !hits.is_empty() {
                let mut req = bot
                    .send_message(tg.chat_id, "pick a contact")
                    .reply_markup(search_keyboard(&hits));
                if let Some(thread) = forum_thread(thread_id) {
                    req = req.message_thread_id(thread);
                }
                req.await.map_err(|e| AppError::Telegram(e.to_string()))?;
            }
        }
        Some("open") => {
            if let Some(contact_id) = parse_open_cmd(text) {
                handle_open(&db, cfg.telegram_group_id, thread_id, contact_id, &tg).await?;
            }
        }
        Some("status") => {
            post_status(&bot, tg.chat_id, thread_id, &db, info.as_ref(), &cfg, None).await?;
        }
        Some(_) => {}
        None if thread_id != GENERAL_THREAD => {
            match handle_owner_text(
                &db,
                &cfg.default_region,
                thread_id,
                text,
                Some(reply_to),
                modem.as_ref(),
                &tg,
                cfg.sms_delete_enabled,
            )
            .await?
            {
                OwnerTextOutcome::NeedNumber(numbers) => {
                    send_number_buttons(&bot, tg.chat_id, thread_id, &numbers).await?;
                }
                OwnerTextOutcome::Done => {}
            }
        }
        None => {}
    }
    Ok(())
}

async fn on_callback(
    bot: Bot,
    q: CallbackQuery,
    cfg: Config,
    db: Arc<Db>,
    modem: Arc<dyn SmsModem>,
    info: Arc<dyn crate::modem::ModemInfo>,
) -> Result<(), AppError> {
    let _ = bot.answer_callback_query(q.id.clone()).await;
    let Some(data) = q.data.as_deref() else {
        return Ok(());
    };
    if q.chat_id()
        .is_some_and(|ChatId(id)| is_owner_dm(&cfg, id, q.from.id.0 as i64))
        && !allow_dm_callback(data)
    {
        return Ok(());
    }
    let thread_id = q
        .regular_message()
        .map(thread_id_i32)
        .unwrap_or(GENERAL_THREAD);
    let tg = RealTg {
        bot: bot.clone(),
        chat_id: ChatId(cfg.telegram_group_id),
    };
    if parse_status_refresh(data) {
        let chat_id = q.chat_id().unwrap_or(tg.chat_id);
        let msg_id = q.regular_message().map(|m| m.id);
        return post_status(&bot, chat_id, thread_id, &db, info.as_ref(), &cfg, msg_id).await;
    }
    if let Some(e164) = parse_num_callback(data) {
        handle_num_callback(
            &db,
            &cfg.default_region,
            thread_id,
            e164,
            Some(modem.as_ref()),
            &tg,
            cfg.sms_delete_enabled,
        )
        .await
    } else if let Some(contact_id) = parse_open_callback(data) {
        handle_open(&db, cfg.telegram_group_id, thread_id, contact_id, &tg).await
    } else {
        Ok(())
    }
}

async fn on_inline_query(bot: Bot, q: InlineQuery, db: Arc<Db>) -> Result<(), AppError> {
    let query = q.query.trim();
    let articles = if query.is_empty() {
        inline_answer_articles(query, Ok(&[]))
    } else {
        match db.search_contacts(query) {
            Ok(hits) => inline_answer_articles(query, Ok(&hits)),
            Err(_) => inline_answer_articles(query, Err(())),
        }
    };
    bot.answer_inline_query(q.id, inline_query_results(&articles))
        .cache_time(0)
        .is_personal(true)
        .await
        .map_err(|e| AppError::Telegram(e.to_string()))?;
    Ok(())
}

pub async fn dispatch(
    cfg: Config,
    db: Arc<Db>,
    modem: Arc<dyn SmsModem>,
    info: Arc<dyn crate::modem::ModemInfo>,
) {
    let bot = Bot::new(cfg.telegram_bot_token.clone());
    register_commands(&bot, cfg.telegram_group_id).await;
    Dispatcher::builder(bot, schema())
        .dependencies(dptree::deps![cfg, db, modem, info])
        .error_handler(LoggingErrorHandler::with_custom_text(
            "telegram handler error",
        ))
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}
