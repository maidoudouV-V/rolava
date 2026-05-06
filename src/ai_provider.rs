pub mod anthropic;
pub mod google_aistudio;
pub mod openai_compatible;
pub mod openrouter;
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

impl MessageRole {
    fn as_openai_compatible_str(&self) -> &'static str {
        match self {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
        }
    }
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

#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// 模型最终返回给用户的主回复文本。
    pub content: String,
    /// 模型的思考内容，部分支持推理输出的服务商会返回。
    pub reasoning_content: Option<String>,
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

// ========== 通用 Provider Trait ==========
#[async_trait]
pub trait AIProvider {
    async fn chat_completions(&self, request_message: &Vec<ContextMessage>)
        -> Result<ChatResponse>;

    async fn describe_image(&self, _image_data_url: &str, _prompt: &str) -> Result<String> {
        anyhow::bail!("当前服务商不支持图片描述")
    }

    async fn web_search(&self, _query: &str) -> Result<String> {
        anyhow::bail!("当前服务商不支持联网搜索")
    }
}
