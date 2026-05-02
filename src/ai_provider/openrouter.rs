use crate::ai_provider::{
    AIProvider,
    ChatResponse,
    ChatUsage,
    ContextMessage,
};
use anyhow::anyhow;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ========== OpenRouter 接口 ==========
pub struct OpenRouterProvider {
    http_client: Client,
    api_key: String,
    base_url: String,
    model: String,
    max_tokens: i32,
    reasoning_effort: String,
}

impl OpenRouterProvider {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        max_tokens: i32,
        reasoning_effort: impl Into<String>,
    ) -> Self {
        Self {
            http_client: Client::new(),
            api_key: api_key.into(),
            base_url: base_url.into(),
            model: model.into(),
            max_tokens,
            reasoning_effort: reasoning_effort.into(),
        }
    }
}

/// OpenRouter Chat Completions 请求结构
#[derive(Serialize)]
struct OpenRouterChatRequest<'a> {
    model: &'a str,
    messages: Vec<OpenRouterChatMessage<'a>>,
    max_completion_tokens: i32,
    reasoning: OpenRouterReasoning<'a>,
    response_format: OpenRouterResponseFormat,
}

#[derive(Serialize)]
struct OpenRouterReasoning<'a> {
    effort: &'a str,
}

#[derive(Serialize)]
struct OpenRouterResponseFormat {
    #[serde(rename = "type")]
    format_type: &'static str,
}

#[derive(Serialize)]
struct OpenRouterChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// OpenRouter Chat Completions 响应结构
#[derive(Deserialize)]
struct OpenRouterChatResponse {
    id: Option<String>,
    model: Option<String>,
    choices: Vec<OpenRouterChoice>,
    usage: Option<OpenRouterUsage>,
}

#[derive(Deserialize)]
struct OpenRouterChoice {
    index: usize,
    message: OpenRouterMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct OpenRouterMessage {
    role: Option<String>,
    content: Option<String>,
    reasoning: Option<Value>,
    reasoning_content: Option<Value>,
    reasoning_details: Option<Value>,
}

#[derive(Deserialize)]
struct OpenRouterUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

#[async_trait]
impl AIProvider for OpenRouterProvider {
    async fn chat_completions(&self, request_messages: &Vec<ContextMessage>) -> anyhow::Result<ChatResponse> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = OpenRouterChatRequest {
            model: &self.model,
            messages: request_messages
                .iter()
                .map(|m| OpenRouterChatMessage {
                    role: m.role.as_openai_compatible_str(),
                    content: &m.content,
                })
                .collect(),
            max_completion_tokens: self.max_tokens,
            reasoning: OpenRouterReasoning {
                effort: &self.reasoning_effort,
            },
            response_format: OpenRouterResponseFormat {
                format_type: "json_object",
            },
        };

        let resp = self
            .http_client
            .post(url)
            .bearer_auth(&self.api_key)
            .header("X-OpenRouter-Title", env!("CARGO_PKG_NAME"))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let error_text = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "OpenRouter API call failed with status {}: {}",
                status,
                error_text
            ));
        }

        let response_text = resp.text().await?;
        let raw_response: Value = serde_json::from_str(&response_text)?;
        let parsed: OpenRouterChatResponse = serde_json::from_str(&response_text)
            .map_err(|e| {
                anyhow!(
                    "failed to parse OpenRouter response: {}\nraw response:\n{}",
                    e,
                    response_text
                )
            })?;
        let content = parsed
            .choices
            .get(0)
            .and_then(|c| c.message.content.clone())
            .ok_or_else(|| anyhow!("empty content in OpenRouter response"))?;

        let reasoning_content = parsed
            .choices
            .get(0)
            .and_then(|c| {
                c.message
                    .reasoning_content
                    .as_ref()
                    .and_then(reasoning_value_to_text)
                    .or_else(|| {
                        c.message
                            .reasoning
                            .as_ref()
                            .and_then(reasoning_value_to_text)
                    })
                    .or_else(|| {
                        c.message
                            .reasoning_details
                            .as_ref()
                            .and_then(reasoning_details_to_text)
                    })
            });
        let finish_reason = parsed
            .choices
            .get(0)
            .and_then(|c| c.finish_reason.clone());

        let usage = parsed.usage.map(|usage| ChatUsage {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
        });

        Ok(ChatResponse {
            content,
            reasoning_content,
            finish_reason,
            id: parsed.id,
            model: parsed.model,
            usage,
            raw_response,
        })
    }
}

fn reasoning_value_to_text(reasoning: &Value) -> Option<String> {
    match reasoning {
        Value::String(text) => non_empty_text(text),
        Value::Array(_) => reasoning_details_to_text(reasoning),
        Value::Object(object) => {
            for key in ["text", "content", "reasoning", "reasoning_content", "summary"] {
                if let Some(text) = object.get(key).and_then(reasoning_value_to_text) {
                    return Some(text);
                }
            }
            Some(reasoning.to_string())
        }
        _ => Some(reasoning.to_string()),
    }
}

fn reasoning_details_to_text(reasoning_details: &Value) -> Option<String> {
    let Value::Array(details) = reasoning_details else {
        return reasoning_value_to_text(reasoning_details);
    };

    let text_blocks: Vec<String> = details
        .iter()
        .filter_map(|detail| {
            detail
                .get("text")
                .and_then(reasoning_value_to_text)
                .or_else(|| detail.get("summary").and_then(reasoning_value_to_text))
        })
        .collect();

    if text_blocks.is_empty() {
        Some(reasoning_details.to_string())
    } else {
        Some(text_blocks.join("\n"))
    }
}

fn non_empty_text(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}
