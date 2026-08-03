pub mod message;
pub mod onebot;

use anyhow::Result;
use async_trait::async_trait;
use std::time::Instant;

use message::MessageTarget;

/// 单次消息发送的行为选项。
#[derive(Debug, Clone, Copy, Default)]
pub struct SendOptions {
    /// 回复延时的计时起点；未指定时从调用发送方法时开始计算。
    pub delay_started_at: Option<Instant>,
}

impl SendOptions {
    pub fn delay_started_now() -> Self {
        Self {
            delay_started_at: Some(Instant::now()),
        }
    }
}

/// 向消息平台投递当前会话的文本，并在成功后完成发送记录持久化。
#[async_trait]
pub trait MessageSender: Send + Sync {
    async fn send_text(
        &self,
        target: &MessageTarget,
        text: &str,
        options: SendOptions,
    ) -> Result<()>;
}
