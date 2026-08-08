use anyhow::Result;

use crate::transport::message::MessageTarget;

/// 与具体工具无关的会话内部触发。
pub struct ConversationTrigger {
    pub user_prompt: String,
}

/// 从应用级服务路由到指定会话的内部触发。
pub struct RoutedConversationTrigger {
    pub target: MessageTarget,
    pub trigger: ConversationTrigger,
}

/// 工具通过该接口重新唤醒当前会话，不依赖 ConversationActor 类型。
pub trait ConversationTriggerSender: Send + Sync {
    fn send_trigger(&self, trigger: ConversationTrigger) -> Result<()>;
}
