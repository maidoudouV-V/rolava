use super::chat_completions as chat;
use crate::ai_provider::{
    AIProvider, ChatUsage, ExtraBody, ReasoningPayload, ReasoningState, ToolChatMessage,
    ToolChatResponse,
};
use crate::request_statistics::{RequestMeasurement, RequestRecorder};
use crate::tools::{ToolCall, ToolDefinition};
use anyhow::{anyhow, bail};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, trace};

/// 视觉识别只做图片转写，固定使用 OpenRouter 支持的最低推理强度。
const VISION_REASONING_EFFORT: &str = "low";
/// OpenRouter 用此 URL 归属请求到应用；必须与标题一同发送。
const OPENROUTER_APP_URL: &str = "https://github.com/maidoudouV-V/rolava";
const OPENROUTER_APP_TITLE: &str = "Rolava";

// ========== OpenRouter 接口 ==========
pub struct OpenRouterProvider {
    http_client: Client,
    api_key: String,
    base_url: String,
    model: String,
    max_tokens: Option<i32>,
    reasoning_effort: String,
    extra_body: ExtraBody,
    request_recorder: Option<RequestRecorder>,
}

impl OpenRouterProvider {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        max_tokens: Option<i32>,
        reasoning_effort: impl Into<String>,
    ) -> Self {
        Self {
            http_client: Client::new(),
            api_key: api_key.into(),
            base_url: base_url.into(),
            model: model.into(),
            max_tokens,
            reasoning_effort: reasoning_effort.into(),
            extra_body: ExtraBody::default(),
            request_recorder: None,
        }
    }

    pub fn with_extra_body(mut self, extra_body: ExtraBody) -> Self {
        self.extra_body = extra_body;
        self
    }

    fn request_body(&self, body: impl Serialize) -> anyhow::Result<Value> {
        self.extra_body.merge(body)
    }

    async fn send_chat_completions(
        &self,
        messages: &[ToolChatMessage],
        tools: &[ToolDefinition],
        session_id: Option<&str>,
    ) -> anyhow::Result<ToolChatResponse> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.request_body(build_openrouter_chat_request(
            &self.model,
            self.max_tokens,
            &self.reasoning_effort,
            messages,
            tools,
            session_id,
        ))?;
        trace!(
            provider = "openrouter",
            model = %self.model,
            request = %serde_json::to_string_pretty(&body)
                .unwrap_or_else(|error| format!("序列化请求失败: {}", error)),
            "AI Provider 完整请求"
        );
        let mut measurement = RequestMeasurement::begin(
            self.request_recorder.as_ref(),
            "openrouter",
            &self.model,
            "chat",
        );
        let result = async {
            let raw_response = self.send_request(&url, &body, &mut measurement).await?;
            parse_openrouter_chat_response_value(raw_response)
        }
        .await;
        measurement.complete(result.is_ok());
        result
    }

    async fn send_request(
        &self,
        url: &str,
        body: &Value,
        measurement: &mut RequestMeasurement,
    ) -> anyhow::Result<Value> {
        let resp = self
            .http_client
            .post(url)
            .bearer_auth(&self.api_key)
            .header("HTTP-Referer", OPENROUTER_APP_URL)
            .header("X-OpenRouter-Title", OPENROUTER_APP_TITLE)
            .json(body)
            .send()
            .await?;
        chat::read_response(resp, measurement, "openrouter", "OpenRouter API").await
    }
}

/// OpenRouter Chat Completions 请求结构
#[derive(Serialize)]
struct OpenRouterChatRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    messages: Vec<OpenRouterChatMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<i32>,
    reasoning: OpenRouterReasoning<'a>,
    response_format: OpenRouterResponseFormat,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<chat::ToolDefinition<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
}

#[derive(Serialize)]
struct OpenRouterReasoning<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<&'a str>,
    summary: &'static str,
}

#[derive(Serialize)]
struct OpenRouterResponseFormat {
    #[serde(rename = "type")]
    format_type: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum OpenRouterChatMessage<'a> {
    System {
        content: &'a str,
    },
    User {
        content: chat::UserContent<'a>,
    },
    Assistant {
        content: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_details: Option<&'a Value>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<chat::RequestToolCall<'a>>,
    },
    Tool {
        tool_call_id: &'a str,
        content: &'a str,
    },
}

/// OpenRouter 视觉描述请求结构
#[derive(Serialize)]
struct OpenRouterVisionRequest<'a> {
    model: &'a str,
    messages: Vec<chat::VisionMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<i32>,
    reasoning: OpenRouterReasoning<'a>,
}

/// OpenRouter Chat Completions 响应结构
#[derive(Deserialize)]
struct OpenRouterChatResponse {
    id: Option<String>,
    model: Option<String>,
    choices: Vec<OpenRouterChoice>,
}

#[derive(Deserialize)]
struct OpenRouterChoice {
    message: OpenRouterMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct OpenRouterMessage {
    content: Option<String>,
    reasoning: Option<Value>,
    reasoning_content: Option<Value>,
    reasoning_details: Option<Value>,
    tool_calls: Option<Vec<chat::ResponseToolCall>>,
}

fn build_openrouter_chat_request<'a>(
    model: &'a str,
    max_tokens: Option<i32>,
    reasoning_effort: &'a str,
    messages: &'a [ToolChatMessage],
    tools: &'a [ToolDefinition],
    session_id: Option<&'a str>,
) -> OpenRouterChatRequest<'a> {
    let messages = messages
        .iter()
        .map(|message| match message {
            ToolChatMessage::System { content } => OpenRouterChatMessage::System {
                content: content.as_str(),
            },
            ToolChatMessage::User { content } => OpenRouterChatMessage::User {
                content: chat::user_content(content),
            },
            ToolChatMessage::Assistant {
                content,
                reasoning,
                tool_calls,
            } => OpenRouterChatMessage::Assistant {
                content: content.as_deref(),
                reasoning: match reasoning {
                    Some(ReasoningPayload::Text(content)) => Some(content.as_str()),
                    Some(ReasoningPayload::Structured(_)) | None => None,
                },
                reasoning_details: match reasoning {
                    Some(ReasoningPayload::Structured(details)) => Some(details),
                    Some(ReasoningPayload::Text(_)) | None => None,
                },
                tool_calls: tool_calls
                    .iter()
                    .map(|call| chat::RequestToolCall {
                        id: call.id.as_str(),
                        call_type: "function",
                        function: chat::RequestFunctionCall {
                            name: call.name.as_str(),
                            arguments: call.arguments.as_str(),
                        },
                    })
                    .collect(),
            },
            ToolChatMessage::Tool {
                tool_call_id,
                content,
            } => OpenRouterChatMessage::Tool {
                tool_call_id: tool_call_id.as_str(),
                content: content.as_str(),
            },
        })
        .collect();
    let tools = (!tools.is_empty()).then(|| {
        tools
            .iter()
            .map(|tool| chat::ToolDefinition {
                definition_type: "function",
                function: chat::FunctionDefinition {
                    name: tool.name,
                    description: tool.description,
                    parameters: &tool.parameters,
                },
            })
            .collect()
    });
    let tool_choice = tools.as_ref().map(|_| "auto");

    OpenRouterChatRequest {
        model,
        session_id,
        messages,
        max_completion_tokens: max_tokens,
        reasoning: OpenRouterReasoning {
            effort: (reasoning_effort != "auto").then_some(reasoning_effort),
            summary: "auto",
        },
        response_format: OpenRouterResponseFormat {
            format_type: "text",
        },
        tools,
        tool_choice,
    }
}

#[async_trait]
impl AIProvider for OpenRouterProvider {
    fn set_request_recorder(&mut self, recorder: RequestRecorder) {
        self.request_recorder = Some(recorder);
    }

    async fn chat_completions(
        &self,
        messages: &[ToolChatMessage],
        tools: &[ToolDefinition],
    ) -> anyhow::Result<ToolChatResponse> {
        self.send_chat_completions(messages, tools, None).await
    }

    async fn chat_completions_with_session(
        &self,
        messages: &[ToolChatMessage],
        tools: &[ToolDefinition],
        session_id: Option<&str>,
    ) -> anyhow::Result<ToolChatResponse> {
        self.send_chat_completions(messages, tools, session_id)
            .await
    }

    async fn describe_image(&self, image_data_url: &str, prompt: &str) -> anyhow::Result<String> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.request_body(OpenRouterVisionRequest {
            model: &self.model,
            messages: vec![chat::VisionMessage {
                role: "user",
                content: vec![
                    chat::VisionContent::Text { text: prompt },
                    chat::VisionContent::ImageUrl {
                        image_url: chat::ImageUrl {
                            url: image_data_url,
                            detail: "high",
                        },
                    },
                ],
            }],
            max_completion_tokens: self.max_tokens,
            reasoning: OpenRouterReasoning {
                effort: Some(VISION_REASONING_EFFORT),
                summary: "auto",
            },
        })?;
        debug!(
            provider = "openrouter",
            model = %self.model,
            prompt,
            image_data_url_bytes = image_data_url.len(),
            "视觉 Provider 请求"
        );

        let mut measurement = RequestMeasurement::begin(
            self.request_recorder.as_ref(),
            "openrouter",
            &self.model,
            "vision",
        );
        let result = async {
            let raw_response = self.send_request(&url, &body, &mut measurement).await?;
            check_openrouter_response_error(&raw_response)?;
            let parsed = OpenRouterChatResponse::deserialize(&raw_response)
                .map_err(|e| anyhow!("解析 OpenRouter 视觉响应失败：{}", e))?;
            parsed
                .choices
                .first()
                .and_then(|c| c.message.content.clone())
                .ok_or_else(|| anyhow!("OpenRouter 视觉响应内容为空"))
        }
        .await;
        measurement.complete(result.is_ok());
        result
    }
}

fn check_openrouter_response_error(raw_response: &Value) -> anyhow::Result<()> {
    let response_error = raw_response
        .get("error")
        .filter(|error| !error.is_null())
        .or_else(|| {
            raw_response
                .get("choices")?
                .as_array()?
                .first()?
                .get("error")
                .filter(|error| !error.is_null())
        });
    if let Some(error) = response_error {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| error.to_string());
        if let Some(code) = error.get("code") {
            bail!("OpenRouter API 返回错误（错误码 {}）：{}", code, message);
        }
        bail!("OpenRouter API 返回错误：{}", message);
    }
    Ok(())
}

fn parse_openrouter_chat_response_value(raw_response: Value) -> anyhow::Result<ToolChatResponse> {
    check_openrouter_response_error(&raw_response)?;
    let parsed: OpenRouterChatResponse = OpenRouterChatResponse::deserialize(&raw_response)
        .map_err(|error| anyhow!("解析 OpenRouter 响应失败：{}", error))?;
    let first_choice = parsed
        .choices
        .first()
        .ok_or_else(|| anyhow!("OpenRouter 响应 choices 为空"))?;
    let content = first_choice.message.content.clone();
    let finish_reason = first_choice.finish_reason.clone();
    let reasoning_details = first_choice.message.reasoning_details.clone();
    let reasoning_text = first_choice
        .message
        .reasoning_content
        .as_ref()
        .and_then(reasoning_value_to_text)
        .or_else(|| {
            first_choice
                .message
                .reasoning
                .as_ref()
                .and_then(reasoning_value_to_text)
        })
        .or_else(|| {
            reasoning_details
                .as_ref()
                .and_then(reasoning_details_to_text)
        });
    let mut tool_calls = Vec::new();
    for call in first_choice
        .message
        .tool_calls
        .as_deref()
        .unwrap_or_default()
    {
        if call.call_type != "function" {
            bail!("OpenRouter 响应包含不支持的工具类型：{}", call.call_type);
        }
        tool_calls.push(ToolCall {
            id: call.id.clone(),
            name: call.function.name.clone(),
            arguments: call.function.arguments.clone(),
        });
    }
    if content.is_none() && tool_calls.is_empty() && finish_reason.as_deref() != Some("stop") {
        bail!(
            "OpenRouter 响应既没有文本内容，也没有 tool_calls，且 finish_reason 不是 stop（当前：{:?}）",
            finish_reason
        );
    }

    let usage = ChatUsage::from_chat_completions(&raw_response);

    Ok(ToolChatResponse {
        content,
        reasoning: ReasoningState {
            display_text: reasoning_text.clone(),
            replay: reasoning_details
                .map(ReasoningPayload::Structured)
                .or_else(|| reasoning_text.map(ReasoningPayload::Text)),
        },
        tool_calls,
        finish_reason,
        id: parsed.id,
        model: parsed.model,
        usage,
        raw_response,
    })
}

fn reasoning_value_to_text(reasoning: &Value) -> Option<String> {
    match reasoning {
        Value::String(text) => non_empty_text(text),
        Value::Array(_) => reasoning_details_to_text(reasoning),
        Value::Object(object) => {
            for key in [
                "text",
                "content",
                "reasoning",
                "reasoning_content",
                "summary",
            ] {
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
