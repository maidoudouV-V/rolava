mod agent_web_search;
mod continue_conversation;
mod create_scheduled_task;
mod delete_scheduled_task;
mod end_conversation;
mod get_scheduled_task;
mod recognize_image;
mod registry;
mod remember;
mod send_message;
mod update_scheduled_task;
mod wait_for_reply;

pub use agent_web_search::{AgentWebSearchArgs, AgentWebSearchTool};
pub use continue_conversation::ContinueConversationTool;
pub use create_scheduled_task::{CreateScheduledTaskArgs, CreateScheduledTaskTool};
pub use delete_scheduled_task::{DeleteScheduledTaskArgs, DeleteScheduledTaskTool};
pub use end_conversation::EndConversationTool;
pub use get_scheduled_task::{GetScheduledTaskArgs, GetScheduledTaskTool};
pub use recognize_image::{RecognizeImageArgs, RecognizeImageTool};
pub use registry::ToolRegistry;
pub use remember::{RememberArgs, RememberTool};
pub use send_message::{SendMessageArgs, SendMessageTool};
pub use update_scheduled_task::{UpdateScheduledTaskArgs, UpdateScheduledTaskTool};
pub use wait_for_reply::{WaitForReplyArgs, WaitForReplyTool};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use crate::config::AppConfig;
use crate::conversation_control::ConversationControl;
use crate::conversation_trigger::ConversationTriggerSender;
use crate::repository::db_manager::QQChatContextManager;
use crate::scheduler::SchedulerService;
use crate::transport::message::{IncomingMessage, MessageTarget};
use crate::transport::MessageSender;

/// Provider 无关的 function tool 定义，由各 AI Provider 转换成自己的请求格式。
#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

/// 模型返回的一次工具调用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// API 返回的原始 JSON 参数字符串。
    pub arguments: String,
}

/// 当前这次工具调用所属的会话信息。
#[derive(Clone)]
pub struct ConversationToolContext {
    pub key: String,
    pub target: MessageTarget,
    pub current_messages: Vec<IncomingMessage>,
    pub control: Arc<ConversationControl>,
    pub trigger_sender: Arc<dyn ConversationTriggerSender>,
}

/// 所有工具共享的应用服务。新增系统能力时统一从这里注入。
#[derive(Clone)]
pub struct ToolServices {
    pub app_config: Arc<AppConfig>,
    pub db_manager: Arc<QQChatContextManager>,
    pub message_sender: Arc<dyn MessageSender>,
    pub scheduler: Arc<SchedulerService>,
}

impl ToolServices {
    pub fn new(
        app_config: Arc<AppConfig>,
        db_manager: Arc<QQChatContextManager>,
        message_sender: Arc<dyn MessageSender>,
        scheduler: Arc<SchedulerService>,
    ) -> Self {
        Self {
            app_config,
            db_manager,
            message_sender,
            scheduler,
        }
    }
}

/// 单次工具调用可使用的完整上下文。
#[derive(Clone)]
pub struct ToolContext {
    pub conversation: ConversationToolContext,
    pub services: Arc<ToolServices>,
}

/// 单个工具处理器的成功输出。
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// 工具产生的结果内容，与是否继续请求 AI 相互独立。
    pub content: String,
    pub requires_ai_response: bool,
}

impl ToolOutput {
    pub fn new(content: impl Into<String>, requires_ai_response: bool) -> Self {
        Self {
            content: content.into(),
            requires_ai_response,
        }
    }

    /// 返回内容并要求模型根据结果继续处理。
    pub fn text(content: impl Into<String>) -> Self {
        Self::new(content, true)
    }

    /// 不返回内容，也不再触发后续模型请求；仅用于结束或继续查看对话。
    pub fn none() -> Self {
        Self::new(String::new(), false)
    }
}

/// 发送回模型的工具调用结果。
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: String,
    pub requires_ai_response: bool,
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

    /// 清除仅属于当前会话生命周期的内存状态；无状态工具无需实现。
    fn reset_conversation_state(&self) {}

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
