use teloxide::prelude::*;
use teloxide::types::{
    ChatId, ForceReply, InlineKeyboardButton, InlineKeyboardMarkup, MessageId, ParseMode,
    ReactionType, ReplyParameters, ThreadId,
};

use crate::app::{AppError, RichPost, TelegramSink};

use super::util::{edit_failed_is_noop, forum_thread};

pub struct RealTg {
    pub bot: teloxide::Bot,
    pub chat_id: ChatId,
}

#[async_trait::async_trait]
impl TelegramSink for RealTg {
    async fn post(&self, thread_id: i32, text: String) -> Result<(), AppError> {
        let mut req = self.bot.send_message(self.chat_id, text);
        if let Some(thread) = forum_thread(thread_id) {
            req = req.message_thread_id(thread);
        }
        req.await.map_err(|e| AppError::Telegram(e.to_string()))?;
        Ok(())
    }

    async fn reply(&self, thread_id: i32, text: String, reply_to: i32) -> Result<(), AppError> {
        let mut req = self
            .bot
            .send_message(self.chat_id, text)
            .reply_parameters(ReplyParameters::new(MessageId(reply_to)));
        if let Some(thread) = forum_thread(thread_id) {
            req = req.message_thread_id(thread);
        }
        req.await.map_err(|e| AppError::Telegram(e.to_string()))?;
        Ok(())
    }

    async fn react(&self, message_id: i32, emoji: &str) -> Result<(), AppError> {
        self.bot
            .set_message_reaction(self.chat_id, MessageId(message_id))
            .reaction(vec![ReactionType::Emoji {
                emoji: emoji.to_string(),
            }])
            .await
            .map_err(|e| AppError::Telegram(e.to_string()))?;
        Ok(())
    }

    async fn create_topic(&self, title: String) -> Result<i32, AppError> {
        let topic = self
            .bot
            .create_forum_topic(self.chat_id, title)
            .await
            .map_err(|e| AppError::Telegram(e.to_string()))?;
        let ThreadId(MessageId(id)) = topic.thread_id;
        Ok(id)
    }

    async fn post_rich(&self, thread_id: i32, post: RichPost) -> Result<i32, AppError> {
        let mut req = self
            .bot
            .send_message(self.chat_id, post.html)
            .parse_mode(ParseMode::Html)
            .disable_notification(post.silent);
        if let Some(thread) = forum_thread(thread_id) {
            req = req.message_thread_id(thread);
        }
        if !post.buttons.is_empty() {
            let row: Vec<_> = post
                .buttons
                .into_iter()
                .map(|(label, data)| InlineKeyboardButton::callback(label, data))
                .collect();
            req = req.reply_markup(InlineKeyboardMarkup::new([row]));
        }
        let msg = req.await.map_err(|e| AppError::Telegram(e.to_string()))?;
        let MessageId(id) = msg.id;
        Ok(id)
    }

    async fn edit_html(&self, message_id: i32, html: String) -> Result<(), AppError> {
        // No reply_markup on an edit removes the inline keyboard.
        self.bot
            .edit_message_text(self.chat_id, MessageId(message_id), html)
            .parse_mode(ParseMode::Html)
            .await
            .map(|_| ())
            .or_else(|e| {
                if edit_failed_is_noop(&e) {
                    Ok(())
                } else {
                    Err(AppError::Telegram(e.to_string()))
                }
            })
    }

    async fn prompt_reply(&self, thread_id: i32, text: String) -> Result<(), AppError> {
        let mut req = self
            .bot
            .send_message(self.chat_id, text)
            .reply_markup(ForceReply::new().selective());
        if let Some(thread) = forum_thread(thread_id) {
            req = req.message_thread_id(thread);
        }
        req.await.map_err(|e| AppError::Telegram(e.to_string()))?;
        Ok(())
    }
}
