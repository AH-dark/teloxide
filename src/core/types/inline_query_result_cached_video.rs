use crate::core::types::{InputMessageContent, InlineKeyboardMarkup, ParseMode};

#[derive(Debug, Serialize, PartialEq, Clone)]
pub struct InlineQueryResultCachedVideo {
    pub id: String,
    pub video_file_id: String,
    pub title: String,
    pub description: Option<String>,
    pub caption: Option<String>,
    pub parse_mode: Option<ParseMode>,
    pub reply_markup: Option<InlineKeyboardMarkup>,
    pub input_message_content: Option<InputMessageContent>,
}