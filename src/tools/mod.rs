mod cancel_scheduled_task;
mod ignore_messages;
mod recognize_image;
mod registry;
mod remember;
mod schedule_task;
mod send_message;
mod wait_then_check;
mod web_search;

pub use cancel_scheduled_task::{CancelScheduledTaskArgs, CancelScheduledTaskTool};
pub use ignore_messages::IgnoreMessagesTool;
pub use recognize_image::{RecognizeImageArgs, RecognizeImageTool};
pub use registry::ToolRegistry;
pub use remember::{RememberArgs, RememberTool};
pub use schedule_task::{ScheduleTaskArgs, ScheduleTaskTool};
pub use send_message::{SendMessageArgs, SendMessageTool};
pub use wait_then_check::{WaitThenCheckArgs, WaitThenCheckTool};
pub use web_search::{WebSearchArgs, WebSearchTool};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;

use crate::transport::message::MessageTarget;
use crate::transport::MessageSender;

/// Provider 无关的 function tool 定义，由各 AI Provider 转换成自己的请求格式。
#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

/// 模型返回的一次工具调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// API 返回的原始 JSON 参数字符串。
    pub arguments: String,
}

/// 当前会话工具共享的运行时依赖。
#[derive(Clone)]
pub struct ToolContext {
    pub conversation_key: String,
    pub message_sender: Arc<dyn MessageSender>,
    pub message_target: MessageTarget,
}

/// 单个工具处理器的成功输出。
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
}

/// 发送回模型的工具调用结果。
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: String,
    pub is_error: bool,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;

    fn description(&self) -> &'static str;

    fn parameters(&self) -> Value;

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name(),
            description: self.description(),
            parameters: self.parameters(),
        }
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput>;
}

pub(crate) fn parse_arguments<T>(tool_name: &str, arguments: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_str(arguments)
        .with_context(|| format!("工具 {} 的参数不是有效 JSON", tool_name))
}

pub(crate) fn execution_not_implemented(tool_name: &str) -> Result<ToolOutput> {
    bail!("工具 {} 尚未接入执行逻辑", tool_name)
}
