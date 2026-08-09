pub mod message;
pub mod onebot;

use anyhow::Result;
use async_trait::async_trait;
use std::time::Instant;

use message::MessageTarget;

/// 一条已经由平台确认发送并完成本地入库的正文。
#[derive(Debug, Clone)]
pub struct SentMessage {
    /// messages 表主键，用于在内存工具历史中定位已经发送的正文。
    pub database_id: i64,
    /// 平台实际收到的分段正文。
    pub text: String,
}

/// 消息平台返回的群聊基础资料。
#[derive(Debug, Clone)]
pub struct GroupInfo {
    pub name: String,
    pub member_count: u64,
}

/// QQ 原生表情动作；随机类表情的实际结果由平台在发送后产生。
#[derive(Debug, Clone)]
pub enum QqExpression {
    Face { id: String, name: String },
    Dice,
    Rps,
}

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
    /// 查询群名称和当前成员数量；不支持或非群聊时返回 None。
    async fn get_group_info(&self, _target: &MessageTarget) -> Result<Option<GroupInfo>> {
        Ok(None)
    }

    /// 发送普通聊天正文，并在平台确认后写入统一聊天记录。
    async fn send_text(
        &self,
        target: &MessageTarget,
        text: &str,
        options: SendOptions,
    ) -> Result<Vec<SentMessage>>;

    /// 发送不会写入聊天记录的控制反馈，例如斜杠命令执行结果。
    async fn send_transient_text(&self, target: &MessageTarget, text: &str) -> Result<()>;

    /// 发送 QQ 原生表情并持久化平台最终返回的内容。
    async fn send_qq_expression(
        &self,
        target: &MessageTarget,
        expression: QqExpression,
        options: SendOptions,
    ) -> Result<SentMessage>;
}
