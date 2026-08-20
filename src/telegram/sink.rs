use teloxide::prelude::*;
use teloxide::types::{ChatId, MessageId, ReactionType, ReplyParameters, ThreadId};

use crate::app::{AppError, TelegramSink};

use super::util::forum_thread;

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
}
