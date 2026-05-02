use anyhow::anyhow;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::ai_provider::{
    AIProvider,
    ChatResponse,
    ChatUsage,
    ContextMessage,
};

/// 视觉识别只做图片转写，固定使用最低推理强度。
const VISION_REASONING_EFFORT: &str = "minimal";

// ========== OpenAI Compatible 接口 ==========
pub struct OpenAICompatibleProvider {
    http_client: Client,
    api_key: String,
    base_url: String,
    model: String,
    max_tokens: i32,
    reasoning_effort: String,
}

impl OpenAICompatibleProvider {
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

/// OpenAI Compatible 请求结构
#[derive(Serialize)]
struct OpenAICompatibleChatRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAICompatibleChatMessage<'a>>,
    max_tokens: i32,
    reasoning_effort: &'a str,
    response_format: OpenAICompatibleResponseFormat,
}

#[derive(Serialize)]
struct OpenAICompatibleResponseFormat {
    #[serde(rename = "type")]
    format_type: &'static str,
}
// 影子结构体直接存 &str
#[derive(Serialize)]
struct OpenAICompatibleChatMessage<'a> {
    role: &'a str, // 这里不存 Role，直接存字符串引用
    content: &'a str,
}

/// OpenAI Compatible 视觉描述请求结构
#[derive(Serialize)]
struct OpenAICompatibleVisionRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAICompatibleVisionMessage<'a>>,
    max_tokens: i32,
    reasoning_effort: &'a str,
}

#[derive(Serialize)]
struct OpenAICompatibleVisionMessage<'a> {
    role: &'a str,
    content: Vec<OpenAICompatibleVisionContent<'a>>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAICompatibleVisionContent<'a> {
    Text {
        text: &'a str,
    },
    ImageUrl {
        image_url: OpenAICompatibleImageUrl<'a>,
    },
}

#[derive(Serialize)]
struct OpenAICompatibleImageUrl<'a> {
    url: &'a str,
}

/// OpenAI Compatible 响应结构
#[derive(Deserialize)]
struct OpenAICompatibleChatResponse {
    id: Option<String>,
    model: Option<String>,
    choices: Vec<OpenAICompatibleChoice>,
    usage: Option<OpenAICompatibleUsage>,
}
#[derive(Deserialize)]
struct OpenAICompatibleChoice {
    index: usize,
    message: OpenAICompatibleMessage,
    finish_reason: Option<String>,
}
#[derive(Deserialize)]
struct OpenAICompatibleMessage {
    role: Option<String>,
    // OpenAI 可能返回 None（例如调用工具时），所以用 Option
    content: Option<String>,
    reasoning_content: Option<String>,
}

#[derive(Deserialize)]
struct OpenAICompatibleUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

#[async_trait]
impl AIProvider for OpenAICompatibleProvider {
    async fn chat_completions(&self, request_messages: &Vec<ContextMessage>) -> anyhow::Result<ChatResponse> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = OpenAICompatibleChatRequest {
            model: &self.model,
            messages: request_messages.iter().map(|m| OpenAICompatibleChatMessage {
                role: m.role.as_openai_compatible_str(),
                content: &m.content,
            }).collect(),
            max_tokens: self.max_tokens,
            reasoning_effort: &self.reasoning_effort,
            response_format: OpenAICompatibleResponseFormat {
                format_type: "json_object",
            },
        };

        let resp = self
            .http_client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        // 处理非 200 响应，保留错误信息
        if !resp.status().is_success() {
            let status = resp.status();
            // 尝试把错误 Body 读出来
            let error_text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("OpenAI Compatible API 调用失败，状态码 {}：{}", status, error_text));
        }
        let response_text = resp.text().await?;
        let raw_response: Value = serde_json::from_str(&response_text)?;
        let parsed: OpenAICompatibleChatResponse = serde_json::from_str(&response_text)
            .map_err(|e| {
                anyhow!(
                    "解析 OpenAI Compatible 响应失败：{}\n原始响应：\n{}",
                    e,
                    response_text
                )
            })?;
        let content = parsed
            .choices
            .get(0)
            .and_then(|c| c.message.content.clone())
            .ok_or_else(|| anyhow!("OpenAI Compatible 响应内容为空"))?;

        let reasoning_content = parsed
            .choices
            .get(0)
            .and_then(|c| c.message.reasoning_content.clone());
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

    async fn describe_image(&self, image_data_url: &str, prompt: &str) -> anyhow::Result<String> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = OpenAICompatibleVisionRequest {
            model: &self.model,
            messages: vec![OpenAICompatibleVisionMessage {
                role: "user",
                content: vec![
                    OpenAICompatibleVisionContent::Text { text: prompt },
                    OpenAICompatibleVisionContent::ImageUrl {
                        image_url: OpenAICompatibleImageUrl {
                            url: image_data_url,
                        },
                    },
                ],
            }],
            max_tokens: self.max_tokens,
            reasoning_effort: VISION_REASONING_EFFORT,
        };

        let resp = self
            .http_client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let error_text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("OpenAI Compatible 视觉 API 调用失败，状态码 {}：{}", status, error_text));
        }

        let response_text = resp.text().await?;
        let parsed: OpenAICompatibleChatResponse = serde_json::from_str(&response_text)
            .map_err(|e| {
                anyhow!(
                    "解析 OpenAI Compatible 视觉响应失败：{}\n原始响应：\n{}",
                    e,
                    response_text
                )
            })?;
        parsed
            .choices
            .get(0)
            .and_then(|c| c.message.content.clone())
            .ok_or_else(|| anyhow!("OpenAI Compatible 视觉响应内容为空"))
    }
}
