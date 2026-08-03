pub mod anthropic;
pub mod google_aistudio;
pub mod openai_compatible;
pub mod openrouter;
use crate::tools::{ToolCall, ToolDefinition, ToolResult};
use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

#[derive(Serialize, Debug, Clone, PartialEq)]
pub enum MessageRole {
    System,
    User,
    Assistant,
}

#[derive(Serialize, Debug, Clone)]
pub struct ContextMessage {
    pub role: MessageRole,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ChatUsage {
    /// 本次请求输入消耗的 token 总数。
    pub prompt_tokens: Option<u64>,
    /// 本次请求输出消耗的 token 总数。
    pub completion_tokens: Option<u64>,
    /// 本次请求总共消耗的 token 数。
    pub total_tokens: Option<u64>,
}

/// 工具调用下一轮需要原样回传的推理载荷。
#[derive(Debug, Clone)]
pub enum ReasoningPayload {
    Text(String),
    Structured(Value),
}

/// 推理信息分为日志展示文本和协议回传载荷，避免展示转换破坏原始数据。
#[derive(Debug, Clone, Default)]
pub struct ReasoningState {
    pub display_text: Option<String>,
    pub replay: Option<ReasoningPayload>,
}

#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// 模型最终返回给用户的主回复文本。
    pub content: String,
    /// 模型返回的推理信息。
    pub reasoning: ReasoningState,
    /// 本次生成结束的原因，例如正常停止或长度截断。
    pub finish_reason: Option<String>,
    /// 服务端为本次响应分配的唯一 ID。
    pub id: Option<String>,
    /// 服务端实际使用并返回的模型名称。
    pub model: Option<String>,
    /// 本次请求的 token 用量统计。
    pub usage: Option<ChatUsage>,
    /// 服务端返回的原始 JSON，便于调试和兼容扩展字段。
    pub raw_response: Value,
}

/// 支持 function tools 的通用对话消息。
#[derive(Debug, Clone)]
pub enum ToolChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        reasoning: Option<ReasoningPayload>,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

impl From<&ContextMessage> for ToolChatMessage {
    fn from(message: &ContextMessage) -> Self {
        match &message.role {
            MessageRole::System => Self::System {
                content: message.content.clone(),
            },
            MessageRole::User => Self::User {
                content: message.content.clone(),
            },
            MessageRole::Assistant => Self::Assistant {
                content: Some(message.content.clone()),
                reasoning: None,
                tool_calls: Vec::new(),
            },
        }
    }
}

impl From<&ToolResult> for ToolChatMessage {
    fn from(result: &ToolResult) -> Self {
        Self::Tool {
            tool_call_id: result.tool_call_id.clone(),
            content: result.content.clone(),
        }
    }
}

/// 一次支持 tools 的模型响应。
#[derive(Debug, Clone)]
pub struct ToolChatResponse {
    pub content: Option<String>,
    pub reasoning: ReasoningState,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub id: Option<String>,
    pub model: Option<String>,
    pub usage: Option<ChatUsage>,
    pub raw_response: Value,
}

impl ToolChatResponse {
    /// 构造下一次请求需要原样回传的 assistant 消息。
    pub fn assistant_message(&self) -> ToolChatMessage {
        ToolChatMessage::Assistant {
            content: self.content.clone(),
            reasoning: self.reasoning.replay.clone(),
            tool_calls: self.tool_calls.clone(),
        }
    }
}

// ========== 通用 Provider Trait ==========
#[async_trait]
pub trait AIProvider {
    /// 统一的聊天请求。tools 为空时是普通文本请求，非空时允许模型返回工具调用。
    async fn chat_completions(
        &self,
        messages: &[ToolChatMessage],
        tools: &[ToolDefinition],
    ) -> Result<ToolChatResponse>;

    async fn describe_image(&self, _image_data_url: &str, _prompt: &str) -> Result<String> {
        anyhow::bail!("当前服务商不支持图片描述")
    }

    async fn web_search(&self, _query: &str) -> Result<String> {
        anyhow::bail!("当前服务商不支持联网搜索")
    }
}

/// 将不含工具历史的统一消息转换给尚未实现 tools 的 Provider 使用。
pub(crate) fn context_messages_without_tools(
    messages: &[ToolChatMessage],
) -> Result<Vec<ContextMessage>> {
    messages
        .iter()
        .map(|message| match message {
            ToolChatMessage::System { content } => Ok(ContextMessage {
                role: MessageRole::System,
                content: content.clone(),
            }),
            ToolChatMessage::User { content } => Ok(ContextMessage {
                role: MessageRole::User,
                content: content.clone(),
            }),
            ToolChatMessage::Assistant {
                content,
                tool_calls,
                ..
            } if tool_calls.is_empty() => Ok(ContextMessage {
                role: MessageRole::Assistant,
                content: content
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("普通 assistant 消息缺少 content"))?,
            }),
            ToolChatMessage::Assistant { .. } | ToolChatMessage::Tool { .. } => {
                anyhow::bail!("当前服务商尚未支持工具调用历史")
            }
        })
        .collect()
}
