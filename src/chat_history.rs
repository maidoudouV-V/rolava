use chrono::{DateTime, Local};

use crate::history_compression::summary_date_for_timestamp;
use crate::repository::db_manager::{
    ChatMessage, ConversationDailySummary, ConversationHistoryWindow, QQChatContextManager,
};

/// 主模型和管理页面共用同一个原始消息窗口及摘要日期范围。
pub struct ChatHistoryContext {
    pub window: ConversationHistoryWindow,
    pub summaries: Vec<ConversationDailySummary>,
}

pub fn load_chat_history_context(
    db_manager: &QQChatContextManager,
    source: &str,
    source_conversation_id: &str,
    max_history_messages: u32,
    now: DateTime<Local>,
    summary_enabled: bool,
) -> anyhow::Result<ChatHistoryContext> {
    let window = db_manager.get_conversation_history_window(
        source,
        source_conversation_id,
        max_history_messages,
    )?;
    let summaries = match window.messages.first() {
        Some(oldest_message) if summary_enabled => {
            let through_date = summary_date_for_timestamp(oldest_message.event_timestamp)?;
            let from_date = summary_date_for_timestamp((now - chrono::Days::new(30)).timestamp())?;
            db_manager.get_conversation_daily_summaries_between(
                oldest_message.conversation_id,
                &from_date,
                &through_date,
            )?
        }
        _ => Vec::new(),
    };
    Ok(ChatHistoryContext { window, summaries })
}

/// 将数据库消息格式化为供模型阅读的一行聊天记录。
pub fn render_history_message_line(message: &ChatMessage, local_time: &DateTime<Local>) -> String {
    let time_text = local_time.format("%H:%M:%S").to_string();
    let sender_name = message
        .sender_nickname
        .clone()
        .unwrap_or_else(|| message.sender_display_name.clone());
    let content = message.content_text.clone().unwrap_or_default();
    format!("{}（{}）:{}", sender_name, time_text, content)
}
