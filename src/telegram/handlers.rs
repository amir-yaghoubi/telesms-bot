use std::sync::Arc;

use teloxide::dispatching::dialogue::GetChatId;
use teloxide::dispatching::{Dispatcher, UpdateHandler};
use teloxide::error_handlers::LoggingErrorHandler;
use teloxide::prelude::*;
use teloxide::types::{BotCommandScope, CallbackQuery, ChatId, InlineQuery, MessageId, ParseMode};

use crate::actions::{
    get_call_forward, ignore, list_numbers, open_topic, put_call_forward, search_contacts,
    send_sms, set_default_number, who, ActionError, Identity,
};
use crate::app::{handle_owner_text, AppError, OwnerTextOutcome, TelegramSink};
use crate::call_forward::CallForwardState;
use crate::config::Config;
use crate::db::{Contact, Db, PendingForward, PendingForwardMode, Topic};
use crate::modem::{CallForward, SmsModem};
use crate::route::GENERAL_THREAD;

use super::keyboards::{
    forward_keyboard, forward_number_keyboard, forward_search_keyboard, inline_answer_articles,
    inline_query_results, number_keyboard, search_keyboard, status_keyboard,
};
use super::parse::{
    allow_dm_callback, allowed, bot_commands, format_who, help_text, is_owner_dm,
    parse_cf_callback, parse_cmd_name, parse_ignore_reply, parse_num_callback, parse_open_callback,
    parse_open_cmd, parse_search_query, parse_sms_cmd, parse_status_refresh, topic_open_message,
    CfAction,
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
    region: &str,
    thread_id: i32,
    tg: &dyn TelegramSink,
) -> Result<Vec<String>, AppError> {
    let numbers = match db.get_topic_by_thread(thread_id)? {
        None => Vec::new(),
        Some(topic) => {
            let identity = Identity {
                contact_id: topic.contact_id,
                number: topic.default_e164.clone(),
                thread_id: Some(thread_id),
            };
            match list_numbers(db, region, &identity) {
                Ok(st) => st.numbers,
                Err(ActionError::NotFound(_)) => Vec::new(),
                Err(ActionError::Db(e)) => return Err(e.into()),
                Err(e) => return Err(AppError::Telegram(e.to_string())),
            }
        }
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
    let Some(topic) = db.get_topic_by_thread(thread_id)? else {
        return Ok(());
    };
    let identity = Identity {
        contact_id: topic.contact_id,
        thread_id: Some(thread_id),
        number: None,
    };
    match set_default_number(db, region, &identity, e164, tg).await {
        Ok(_) => {}
        Err(ActionError::Db(e)) => return Err(e.into()),
        Err(e) => return Err(AppError::Telegram(e.to_string())),
    }
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
    match open_topic(
        db,
        "",
        &Identity {
            contact_id: Some(contact_id),
            ..Default::default()
        },
        tg,
    )
    .await
    {
        Ok(opened) => {
            let topic = db
                .get_topic_by_thread(opened.thread_id)?
                .ok_or_else(|| AppError::Telegram("topic missing after open".into()))?;
            tg.post(reply_thread, topic_open_message(group_id, &topic))
                .await?;
            Ok(())
        }
        Err(ActionError::NotFound(_)) => {
            tg.post(reply_thread, "unknown contact".to_string()).await?;
            Ok(())
        }
        Err(ActionError::Db(e)) => Err(e.into()),
        Err(e) => Err(AppError::Telegram(e.to_string())),
    }
}

pub(crate) async fn handle_ignore(
    db: &Db,
    region: &str,
    thread_id: i32,
    reply_text: Option<&str>,
    tg: &dyn TelegramSink,
) -> Result<(), AppError> {
    let identity = if thread_id == GENERAL_THREAD {
        Identity {
            number: reply_text.and_then(parse_ignore_reply).map(str::to_string),
            ..Default::default()
        }
    } else if let Some(topic) = db.get_topic_by_thread(thread_id)? {
        Identity {
            contact_id: topic.contact_id,
            thread_id: Some(thread_id),
            ..Default::default()
        }
    } else {
        Identity::default()
    };

    const IGNORE_HINT: &str = "reply to a +number to ignore it";
    match ignore(db, region, &identity, tg).await {
        Ok(_) => Ok(()),
        Err(ActionError::Validation(_)) | Err(ActionError::MissingIdentity)
            if thread_id == GENERAL_THREAD =>
        {
            tg.post(thread_id, IGNORE_HINT.to_string()).await?;
            Ok(())
        }
        Err(ActionError::Validation(_)) => {
            tg.post(thread_id, IGNORE_HINT.to_string()).await?;
            Ok(())
        }
        Err(ActionError::Db(e)) => Err(e.into()),
        Err(e) => Err(AppError::Telegram(e.to_string())),
    }
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
    match send_sms(
        db,
        region,
        &Identity {
            number: Some(raw_number.to_string()),
            ..Default::default()
        },
        text,
        reply_thread,
        reply_to,
        modem,
        tg,
        delete_enabled,
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(ActionError::InvalidNumber(msg)) => {
            tg.post(reply_thread, msg).await?;
            Ok(())
        }
        Err(ActionError::ModemFailed(_)) => Ok(()),
        Err(ActionError::Db(e)) => Err(e.into()),
        Err(e) => Err(AppError::Telegram(e.to_string())),
    }
}

pub(crate) struct PendingForwardTextResult {
    pub pending: PendingForward,
    pub state: Option<CallForwardState>,
    pub hits: Vec<Contact>,
    pub message: String,
}

pub(crate) async fn handle_pending_forward_text(
    db: &Db,
    region: &str,
    thread_id: i32,
    text: &str,
    forward: &dyn CallForward,
) -> Result<Option<PendingForwardTextResult>, AppError> {
    let Some(pending) = db.take_pending_forward(thread_id)? else {
        return Ok(None);
    };
    let result = match pending.mode {
        PendingForwardMode::Number => {
            match put_call_forward(forward, region, Some(text.into())).await {
                Ok(state) => PendingForwardTextResult {
                    pending,
                    state: Some(state),
                    hits: Vec::new(),
                    message: String::new(),
                },
                Err(ActionError::Db(err)) => return Err(err.into()),
                Err(err) => PendingForwardTextResult {
                    pending,
                    state: None,
                    hits: Vec::new(),
                    message: format!("Could not set forwarding: {err}"),
                },
            }
        }
        PendingForwardMode::Search => match search_contacts(db, text) {
            Ok(hits) if hits.is_empty() => PendingForwardTextResult {
                pending,
                state: None,
                hits,
                message: "No matching contacts.".into(),
            },
            Ok(hits) => PendingForwardTextResult {
                pending,
                state: None,
                hits,
                message: "Pick a contact.".into(),
            },
            Err(ActionError::Db(err)) => return Err(err.into()),
            Err(err) => PendingForwardTextResult {
                pending,
                state: None,
                hits: Vec::new(),
                message: format!("Could not search contacts: {err}"),
            },
        },
    };
    Ok(Some(result))
}

fn forward_card_text(db: &Db, state: &CallForwardState) -> Result<String, AppError> {
    let line = if state.enabled {
        let e164 = state.e164.as_deref().unwrap_or("unknown");
        let label = db
            .find_contact_by_e164(e164)?
            .map(|contact| contact.display_name)
            .unwrap_or_else(|| e164.to_string());
        format!("↪️ Forward · {}", crate::status::html_escape(&label))
    } else {
        "↪️ Forward · off".into()
    };
    Ok(format!(
        "<b>Call forwarding</b>\n{line}\n\nChoose an action."
    ))
}

async fn edit_forward_card(
    bot: &Bot,
    chat_id: ChatId,
    message_id: MessageId,
    text: String,
    keyboard: teloxide::types::InlineKeyboardMarkup,
) -> Result<(), AppError> {
    let request = bot
        .edit_message_text(chat_id, message_id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboard);
    match request.await {
        Ok(_) => Ok(()),
        Err(err) if edit_failed_is_noop(&err) => Ok(()),
        Err(err) => Err(AppError::Telegram(err.to_string())),
    }
}

async fn edit_forward_result(
    bot: &Bot,
    db: &Db,
    chat_id: ChatId,
    message_id: MessageId,
    result: Result<CallForwardState, ActionError>,
) -> Result<(), AppError> {
    match result {
        Ok(state) => {
            edit_forward_card(
                bot,
                chat_id,
                message_id,
                forward_card_text(db, &state)?,
                forward_keyboard(),
            )
            .await
        }
        Err(ActionError::Db(err)) => Err(err.into()),
        Err(err) => {
            let text = format!(
                "<b>Call forwarding</b>\n{}\n\nChoose an action.",
                crate::status::html_escape(&format!("Operation failed: {err}"))
            );
            edit_forward_card(bot, chat_id, message_id, text, forward_keyboard()).await
        }
    }
}

async fn send_forward_card(
    bot: &Bot,
    chat_id: ChatId,
    thread_id: i32,
    db: &Db,
    forward: &dyn CallForward,
    region: &str,
) -> Result<(), AppError> {
    let state = match get_call_forward(forward, region).await {
        Ok(state) => state,
        Err(err) => {
            let text = format!("Call forwarding unavailable: {err}");
            let mut request = bot.send_message(chat_id, text);
            if let Some(thread) = forum_thread(thread_id) {
                request = request.message_thread_id(thread);
            }
            request
                .await
                .map_err(|error| AppError::Telegram(error.to_string()))?;
            return Ok(());
        }
    };
    let mut request = bot
        .send_message(chat_id, forward_card_text(db, &state)?)
        .parse_mode(ParseMode::Html)
        .reply_markup(forward_keyboard());
    if let Some(thread) = forum_thread(thread_id) {
        request = request.message_thread_id(thread);
    }
    request
        .await
        .map_err(|error| AppError::Telegram(error.to_string()))?;
    Ok(())
}

async fn post_status(
    bot: &Bot,
    chat_id: ChatId,
    thread_id: i32,
    db: &Db,
    info: &dyn crate::modem::ModemInfo,
    forward: &dyn CallForward,
    cfg: &Config,
    edit: Option<MessageId>,
) -> Result<(), AppError> {
    let html =
        match crate::status::gather(
            info,
            forward,
            &cfg.default_region,
            db,
            cfg.status_tz,
            &cfg.modem_uid,
            chrono::Utc::now(),
        )
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
    forward: Arc<dyn CallForward>,
) -> Result<(), AppError> {
    if parse_cmd_name(msg.text().unwrap_or("")) == Some("status") {
        return post_status(
            &bot,
            msg.chat.id,
            crate::route::GENERAL_THREAD,
            &db,
            info.as_ref(),
            forward.as_ref(),
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
    forward: Arc<dyn CallForward>,
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
    if let Some(result) =
        handle_pending_forward_text(&db, &cfg.default_region, thread_id, text, forward.as_ref())
            .await?
    {
        let chat_id = ChatId(result.pending.edit_chat_id);
        let message_id = MessageId(result.pending.edit_message_id);
        if let Some(state) = result.state {
            edit_forward_card(
                &bot,
                chat_id,
                message_id,
                forward_card_text(&db, &state)?,
                forward_keyboard(),
            )
            .await?;
        } else {
            let keyboard = if result.hits.is_empty() {
                forward_keyboard()
            } else {
                forward_search_keyboard(&result.hits)
            };
            let text = format!(
                "<b>Call forwarding</b>\n{}\n\nChoose an action.",
                crate::status::html_escape(&result.message)
            );
            edit_forward_card(&bot, chat_id, message_id, text, keyboard).await?;
        }
        return Ok(());
    }
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
            let numbers =
                handle_number_empty_or_list(&db, &cfg.default_region, thread_id, &tg).await?;
            if !numbers.is_empty() {
                send_number_buttons(&bot, tg.chat_id, thread_id, &numbers).await?;
            }
        }
        Some("ignore") => {
            let reply = msg.reply_to_message().and_then(|m| m.text());
            handle_ignore(&db, &cfg.default_region, thread_id, reply, &tg).await?;
        }
        Some("forward") => {
            send_forward_card(
                &bot,
                tg.chat_id,
                thread_id,
                &db,
                forward.as_ref(),
                &cfg.default_region,
            )
            .await?;
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
            post_status(
                &bot,
                tg.chat_id,
                thread_id,
                &db,
                info.as_ref(),
                forward.as_ref(),
                &cfg,
                None,
            )
            .await?;
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
    forward: Arc<dyn CallForward>,
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
        return post_status(
            &bot,
            chat_id,
            thread_id,
            &db,
            info.as_ref(),
            forward.as_ref(),
            &cfg,
            msg_id,
        )
        .await;
    }
    if let Some(action) = parse_cf_callback(data) {
        let Some(message) = q.regular_message() else {
            return Ok(());
        };
        let chat_id = message.chat.id;
        let message_id = message.id;
        match action {
            CfAction::TypeNumber => {
                db.set_pending_forward(
                    thread_id,
                    PendingForwardMode::Number,
                    chat_id.0,
                    message_id.0,
                )?;
                edit_forward_card(
                    &bot,
                    chat_id,
                    message_id,
                    "<b>Call forwarding</b>\nSend the number to forward calls to.".to_string(),
                    forward_keyboard(),
                )
                .await
            }
            CfAction::Search => {
                db.set_pending_forward(
                    thread_id,
                    PendingForwardMode::Search,
                    chat_id.0,
                    message_id.0,
                )?;
                edit_forward_card(
                    &bot,
                    chat_id,
                    message_id,
                    "<b>Call forwarding</b>\nSend a contact name to search.".to_string(),
                    forward_keyboard(),
                )
                .await
            }
            CfAction::Disable => {
                db.clear_pending_forward(thread_id)?;
                edit_forward_result(
                    &bot,
                    &db,
                    chat_id,
                    message_id,
                    put_call_forward(forward.as_ref(), &cfg.default_region, None).await,
                )
                .await
            }
            CfAction::Cancel => {
                db.clear_pending_forward(thread_id)?;
                edit_forward_result(
                    &bot,
                    &db,
                    chat_id,
                    message_id,
                    get_call_forward(forward.as_ref(), &cfg.default_region).await,
                )
                .await
            }
            CfAction::Contact(contact_id) => {
                let Some(contact) = db.get_contact(contact_id)? else {
                    return edit_forward_card(
                        &bot,
                        chat_id,
                        message_id,
                        "<b>Call forwarding</b>\nUnknown contact.\n\nChoose an action.".to_string(),
                        forward_keyboard(),
                    )
                    .await;
                };
                match contact.numbers.as_slice() {
                    [] => {
                        edit_forward_card(
                            &bot,
                            chat_id,
                            message_id,
                            "<b>Call forwarding</b>\nThis contact has no phone numbers.\n\nChoose an action."
                                .to_string(),
                            forward_keyboard(),
                        )
                        .await
                    }
                    [e164] => {
                        edit_forward_result(
                            &bot,
                            &db,
                            chat_id,
                            message_id,
                            put_call_forward(
                                forward.as_ref(),
                                &cfg.default_region,
                                Some(e164.clone()),
                            )
                            .await,
                        )
                        .await
                    }
                    numbers => {
                        edit_forward_card(
                            &bot,
                            chat_id,
                            message_id,
                            format!(
                                "<b>Call forwarding</b>\nPick a number for {}.",
                                crate::status::html_escape(&contact.display_name)
                            ),
                            forward_number_keyboard(numbers),
                        )
                        .await
                    }
                }
            }
            CfAction::Number(e164) => {
                edit_forward_result(
                    &bot,
                    &db,
                    chat_id,
                    message_id,
                    put_call_forward(forward.as_ref(), &cfg.default_region, Some(e164)).await,
                )
                .await
            }
        }
    } else if let Some(e164) = parse_num_callback(data) {
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
        match search_contacts(&db, query) {
            Ok(hits) => inline_answer_articles(query, Ok(&hits)),
            Err(ActionError::ContactsUnavailable | ActionError::Validation(_)) => {
                inline_answer_articles(query, Err(()))
            }
            Err(ActionError::Db(e)) => return Err(e.into()),
            Err(e) => return Err(AppError::Telegram(e.to_string())),
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
    forward: Arc<dyn CallForward>,
) {
    let bot = Bot::new(cfg.telegram_bot_token.clone());
    register_commands(&bot, cfg.telegram_group_id).await;
    Dispatcher::builder(bot, schema())
        .dependencies(dptree::deps![cfg, db, modem, info, forward])
        .error_handler(LoggingErrorHandler::with_custom_text(
            "telegram handler error",
        ))
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}
