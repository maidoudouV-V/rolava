use anyhow::Result;

use crate::transport::message::IncomingMessage;

/// 与具体工具无关的会话内部触发。
pub struct ConversationTrigger {
    pub conversation_messages: Vec<IncomingMessage>,
    pub user_prompt: String,
}

/// 工具通过该接口重新唤醒当前会话，不依赖 ConversationActor 类型。
pub trait ConversationTriggerSender: Send + Sync {
    fn send_trigger(&self, trigger: ConversationTrigger) -> Result<()>;
}
