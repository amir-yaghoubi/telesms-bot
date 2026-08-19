use teloxide::types::{Message, MessageId, ThreadId};
use teloxide::{ApiError, RequestError};

use crate::route::GENERAL_THREAD;

pub(crate) fn thread_id_i32(msg: &Message) -> i32 {
    msg.thread_id
        .map(|ThreadId(MessageId(id))| id)
        .unwrap_or(GENERAL_THREAD)
}

/// Telegram's General topic is not a real thread. Sending `message_thread_id=1`
/// fails with "message thread not found". Omit the field for General.
pub(crate) fn forum_thread(thread_id: i32) -> Option<ThreadId> {
    if thread_id == GENERAL_THREAD {
        None
    } else {
        Some(ThreadId(MessageId(thread_id)))
    }
}

pub(crate) fn edit_failed_is_noop(err: &RequestError) -> bool {
    match err {
        RequestError::Api(ApiError::MessageNotModified) => true,
        _ => err.to_string().contains("message is not modified"),
    }
}
