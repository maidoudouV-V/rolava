mod agent_web_search;
mod continue_conversation;
mod create_scheduled_task;
mod create_user_memory;
mod delete_character_memory;
mod delete_scheduled_task;
mod delete_user_memory;
mod end_conversation;
mod get_scheduled_task;
mod registry;
mod send_message;
mod send_qq_expression;
mod set_character_memory;
mod update_scheduled_task;
mod update_user_memory;
mod wait_for_reply;

pub use agent_web_search::AgentWebSearchTool;
pub use continue_conversation::ContinueConversationTool;
pub use create_scheduled_task::CreateScheduledTaskTool;
pub use create_user_memory::CreateUserMemoryTool;
pub use delete_character_memory::DeleteCharacterMemoryTool;
pub use delete_scheduled_task::DeleteScheduledTaskTool;
pub use delete_user_memory::DeleteUserMemoryTool;
pub use end_conversation::EndConversationTool;
pub use get_scheduled_task::GetScheduledTaskTool;
pub use registry::{OptionalToolDefinition, ToolRegistry};
pub use send_qq_expression::SendQqExpressionTool;
pub use set_character_memory::SetCharacterMemoryTool;
pub use update_scheduled_task::UpdateScheduledTaskTool;
pub use update_user_memory::UpdateUserMemoryTool;
pub use wait_for_reply::WaitForReplyTool;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use crate::config::AppConfig;
use crate::conversation_control::ConversationControl;
use crate::conversation_trigger::ConversationTriggerSender;
use crate::memory::{CharacterMemorySession, UserMemorySession};
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
    pub character_memory: Arc<CharacterMemorySession>,
    pub user_memory: Arc<UserMemorySession>,
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
    /// 工具对当前对话生命周期产生的通用影响。
    pub conversation_effect: ConversationEffect,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConversationEffect {
    #[default]
    None,
    Continue,
    End,
}

impl ToolOutput {
    pub fn new(content: impl Into<String>, requires_ai_response: bool) -> Self {
        Self {
            content: content.into(),
            requires_ai_response,
            conversation_effect: ConversationEffect::None,
        }
    }

    /// 返回内容并要求模型根据结果继续处理。
    pub fn text(content: impl Into<String>) -> Self {
        Self::new(content, true)
    }

    pub fn end_conversation() -> Self {
        Self {
            content: String::new(),
            requires_ai_response: false,
            conversation_effect: ConversationEffect::End,
        }
    }

    pub fn continue_conversation() -> Self {
        Self {
            content: String::new(),
            requires_ai_response: false,
            conversation_effect: ConversationEffect::Continue,
        }
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
    pub conversation_effect: ConversationEffect,
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
