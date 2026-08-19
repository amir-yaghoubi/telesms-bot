use teloxide::types::BotCommand;

use crate::config::Config;
use crate::db::Topic;

pub fn allowed(cfg: &Config, chat_id: i64, user_id: i64) -> bool {
    chat_id == cfg.telegram_group_id && user_id == cfg.telegram_user_id
}

/// Owner DMs the bot (private chat id equals user id). Not the forum group.
pub fn is_owner_dm(cfg: &Config, chat_id: i64, user_id: i64) -> bool {
    user_id == cfg.telegram_user_id && chat_id == user_id
}

pub fn parse_sms_cmd(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix("/sms")?;
    let rest = if let Some(after_at) = rest.strip_prefix('@') {
        after_at.split_once(char::is_whitespace)?.1
    } else {
        rest
    };
    let rest = rest.trim();
    let (num, body) = rest.split_once(char::is_whitespace)?;
    let body = body.trim();
    if num.is_empty() || body.is_empty() {
        return None;
    }
    Some((num.to_string(), body.to_string()))
}

pub fn format_who(topic: &Topic, name: Option<&str>, numbers: &[String]) -> String {
    let mut out = name.unwrap_or(topic.title.as_str()).to_string();
    for n in numbers {
        out.push('\n');
        out.push_str(n);
        if topic.default_e164.as_deref() == Some(n.as_str()) {
            out.push_str(" (default)");
        }
    }
    out
}

pub(crate) fn parse_cmd_name(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('/')?;
    let token = rest.split_whitespace().next()?;
    let name = token.split('@').next()?;
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

pub fn bot_commands() -> Vec<BotCommand> {
    vec![
        BotCommand::new("help", "How to send SMS and use this group"),
        BotCommand::new("sms", "Send SMS: /sms <number> <text>"),
        BotCommand::new("search", "Find a contact and open their topic"),
        BotCommand::new("who", "Show this topic's contact and numbers"),
        BotCommand::new("number", "Choose the default number for this topic"),
        BotCommand::new("ignore", "Stop auto-creating a topic for this number"),
        BotCommand::new("status", "Modem and gateway status"),
    ]
}

pub fn help_text() -> &'static str {
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

/status
  Modem, SIM, today's SMS counts, last in/out, contacts.
  Works in any topic and in a private chat with the bot.

Typing in a contact topic sends SMS to that contact's default number.
Text in General is not an SMS unless you use /sms."
}

pub fn parse_status_refresh(data: &str) -> bool {
    data == "st:r"
}

pub fn allow_dm_callback(data: &str) -> bool {
    parse_status_refresh(data)
}

pub fn parse_num_callback(data: &str) -> Option<&str> {
    let e164 = data.strip_prefix("num:")?;
    if e164.is_empty() {
        None
    } else {
        Some(e164)
    }
}

pub fn parse_search_query(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("/search")?;
    let rest = if let Some(after_at) = rest.strip_prefix('@') {
        after_at.split_once(char::is_whitespace)?.1
    } else if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        rest
    } else {
        return None;
    };
    let q = rest.trim();
    if q.is_empty() {
        None
    } else {
        Some(q)
    }
}

pub fn parse_open_callback(data: &str) -> Option<i64> {
    data.strip_prefix("open:")?.parse().ok()
}

pub fn parse_open_cmd(text: &str) -> Option<i64> {
    let rest = text.strip_prefix("/open")?;
    let rest = if let Some(after_at) = rest.strip_prefix('@') {
        after_at.split_once(char::is_whitespace)?.1
    } else if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        rest
    } else {
        return None;
    };
    let id = rest.trim();
    if id.is_empty() {
        None
    } else {
        id.parse().ok()
    }
}

pub fn topic_open_message(group_id: i64, topic: &Topic) -> String {
    let s = group_id.to_string();
    match s.strip_prefix("-100") {
        Some(inner) if !inner.is_empty() => {
            format!("open topic\nhttps://t.me/c/{inner}/{}", topic.thread_id)
        }
        _ => format!("topic exists: {}", topic.title),
    }
}

pub fn parse_ignore_reply(text: &str) -> Option<&str> {
    let first = text.lines().next()?.trim();
    if !first.starts_with('+') {
        return None;
    }
    first.split_whitespace().next()
}
