pub mod message;
pub mod onebot;

use anyhow::Result;
use async_trait::async_trait;

use message::MessageTarget;

/// 向消息平台投递当前会话的文本，并在成功后完成发送记录持久化。
#[async_trait]
pub trait MessageSender: Send + Sync {
    async fn send_text(&self, target: &MessageTarget, text: &str) -> Result<()>;
}
