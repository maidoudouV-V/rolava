use chrono::{DateTime, Local};

use crate::repository::db_manager::ChatMessage;

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
