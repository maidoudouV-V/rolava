use anyhow::Result;

use crate::transport::message::MessageTarget;

/// 与具体工具无关的会话内部触发。
pub struct ConversationTrigger {
    pub user_prompt: String,
    pub memory_review: bool,
    /// 在会话实际处理触发时检查，避免排队期间条件已经失效。
    pub condition: Option<Box<dyn FnOnce() -> Result<bool> + Send + Sync>>,
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
