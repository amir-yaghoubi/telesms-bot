use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, InlineQueryResult, InlineQueryResultArticle,
    InputMessageContent, InputMessageContentText,
};

use crate::db::Contact;

pub fn inline_articles(hits: &[Contact]) -> Vec<(String, String, String)> {
    hits.iter()
        .take(20)
        .map(|c| {
            let id = c.id.to_string();
            let title = c.display_name.clone();
            let description = c.numbers.first().cloned().unwrap_or_default();
            (id, title, description)
        })
        .collect()
}

pub fn inline_answer_articles(
    query: &str,
    search: Result<&[Contact], ()>,
) -> Vec<(String, String, String)> {
    if query.trim().is_empty() {
        Vec::new()
    } else {
        match search {
            Ok(hits) => inline_articles(hits),
            Err(()) => vec![(
                "unavailable".into(),
                "contacts unavailable".into(),
                String::new(),
            )],
        }
    }
}

pub(crate) fn inline_query_results(
    articles: &[(String, String, String)],
) -> Vec<InlineQueryResult> {
    articles
        .iter()
        .map(|(id, title, description)| {
            let text = if id == "unavailable" {
                "contacts unavailable".to_string()
            } else {
                format!("/open {id}")
            };
            InlineQueryResult::Article(
                InlineQueryResultArticle::new(
                    id.clone(),
                    title.clone(),
                    InputMessageContent::Text(InputMessageContentText::new(text)),
                )
                .description(description.clone()),
            )
        })
        .collect()
}

pub(crate) fn status_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([[InlineKeyboardButton::callback("🔄 Refresh", "st:r")]])
}

pub(crate) fn number_keyboard(numbers: &[String]) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(numbers.iter().map(|n| {
        [InlineKeyboardButton::callback(
            n.clone(),
            format!("num:{n}"),
        )]
    }))
}

fn button_label(name: &str) -> String {
    const MAX: usize = 64;
    let mut label: String = name.chars().take(MAX).collect();
    if label.is_empty() {
        label.push('?');
    }
    label
}

pub(crate) fn search_keyboard(hits: &[Contact]) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(hits.iter().take(20).map(|c| {
        [InlineKeyboardButton::callback(
            button_label(&c.display_name),
            format!("open:{}", c.id),
        )]
    }))
}
