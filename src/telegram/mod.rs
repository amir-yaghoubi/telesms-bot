mod handlers;
mod keyboards;
mod parse;
mod sink;
mod util;

pub use handlers::{dispatch, handle_sms, schema};
pub use keyboards::{inline_answer_articles, inline_articles};
pub use parse::{
    allow_dm_callback, allowed, bot_commands, format_who, help_text, is_otp_teach_callback,
    is_owner_dm, parse_cf_callback, parse_ignore_reply, parse_num_callback, parse_open_callback,
    parse_open_cmd, parse_otp_cmd, parse_search_query, parse_sms_cmd, parse_status_refresh,
    parse_teach_prompt, teach_prompt, topic_open_message, CfAction,
};
pub use sink::RealTg;

#[cfg(test)]
mod tests;
