use anyhow::anyhow;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use crate::ai_provider::{AIProvider, ChatResponse, ContextMessage};

// ========== Anthropic 接口骨架 ==========
pub struct AnthropicProvider {
    http_client: Client,
    api_key: String,
    base_url: String,
    model: String,
    max_tokens: i32,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            http_client: Client::new(),
            api_key: api_key.into(),
            base_url: base_url.into(),
            model: model.into(),
            max_tokens: 10240,
        }
    }
}

/// Anthropic 请求结构
#[derive(Serialize)]
struct AnthropicChatRequest<'a> {
    model: &'a str,
    messages: Vec<AnthropicChatMessage<'a>>,
    max_tokens: i32,
}

#[derive(Serialize)]
struct AnthropicChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// Anthropic 响应结构
#[derive(Deserialize)]
struct AnthropicChatResponse {
    content: Vec<AnthropicContentBlock>,
}

#[derive(Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
}

#[async_trait]
impl AIProvider for AnthropicProvider {
    async fn chat_completions(&self, _request_messages: &Vec<ContextMessage>) -> anyhow::Result<ChatResponse> {
        Err(anyhow!("Anthropic provider is not implemented yet"))
    }
}
