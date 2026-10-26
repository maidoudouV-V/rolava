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

/// 视觉识别只做图片转写，固定使用最低推理强度。
const VISION_REASONING_EFFORT: &str = "minimal";
/// 官方 Web Search 的平衡档搜索上下文大小。
const WEB_SEARCH_CONTEXT_SIZE: &str = "medium";

// ========== OpenAI Compatible 接口 ==========
pub struct OpenAICompatibleProvider {
    http_client: Client,
    api_key: String,
    base_url: String,
    model: String,
    max_tokens: Option<i32>,
    reasoning_effort: String,
    extra_body: ExtraBody,
    request_recorder: Option<RequestRecorder>,
}

impl OpenAICompatibleProvider {
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

    async fn send_request(
        &self,
        body: &Value,
        api_name: &str,
        measurement: &mut RequestMeasurement,
    ) -> anyhow::Result<Value> {
        let url = format!("{}/chat/completions", self.base_url);
        let resp = self
            .http_client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await?;
        chat::read_response(resp, measurement, "openai_compatible", api_name).await
    }
}

/// OpenAI Compatible 聊天请求；tools 为空时省略工具相关字段。
#[derive(Serialize)]
struct OpenAICompatibleChatRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAICompatibleToolMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<chat::ToolDefinition<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
}

#[derive(Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum OpenAICompatibleToolMessage<'a> {
    System {
        content: &'a str,
    },
    User {
        content: chat::UserContent<'a>,
    },
    Assistant {
        content: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<&'a str>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<chat::RequestToolCall<'a>>,
    },
    Tool {
        tool_call_id: &'a str,
        content: &'a str,
    },
}

/// OpenAI Compatible 视觉描述请求结构
#[derive(Serialize)]
struct OpenAICompatibleVisionRequest<'a> {
    model: &'a str,
    messages: Vec<chat::VisionMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<i32>,
    reasoning_effort: &'a str,
}

/// OpenAI Chat Completions 官方 Web Search 请求结构。
#[derive(Serialize)]
struct OpenAICompatibleWebSearchRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAICompatibleToolMessage<'a>>,
    web_search_options: OpenAICompatibleWebSearchOptions,
}

#[derive(Serialize)]
struct OpenAICompatibleWebSearchOptions {
    search_context_size: &'static str,
}

/// OpenAI Compatible 响应结构
#[derive(Deserialize)]
struct OpenAICompatibleChatResponse {
    id: Option<String>,
    model: Option<String>,
    choices: Option<Vec<OpenAICompatibleChoice>>,
}
#[derive(Deserialize)]
struct OpenAICompatibleChoice {
    message: OpenAICompatibleMessage,
    finish_reason: Option<String>,
}
#[derive(Deserialize)]
struct OpenAICompatibleMessage {
    // OpenAI 可能返回 None（例如调用工具时），所以用 Option
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<chat::ResponseToolCall>>,
}

fn build_openai_compatible_request<'a>(
    model: &'a str,
    max_tokens: Option<i32>,
    reasoning_effort: &'a str,
    messages: &'a [ToolChatMessage],
    tools: &'a [ToolDefinition],
) -> OpenAICompatibleChatRequest<'a> {
    let messages = messages
        .iter()
        .map(|message| match message {
            ToolChatMessage::System { content } => OpenAICompatibleToolMessage::System {
                content: content.as_str(),
            },
            ToolChatMessage::User { content } => OpenAICompatibleToolMessage::User {
                content: chat::user_content(content),
            },
            ToolChatMessage::Assistant {
                content,
                reasoning,
                tool_calls,
            } => OpenAICompatibleToolMessage::Assistant {
                content: content.as_deref(),
                // OpenAI Compatible 接口默认使用 reasoning_content 回传文本推理内容。
                reasoning_content: match reasoning {
                    Some(ReasoningPayload::Text(content)) => Some(content.as_str()),
                    Some(ReasoningPayload::Structured(_)) | None => None,
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
            } => OpenAICompatibleToolMessage::Tool {
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

    OpenAICompatibleChatRequest {
        model,
        messages,
        max_tokens,
        reasoning_effort: (reasoning_effort != "auto").then_some(reasoning_effort),
        tools,
        tool_choice,
    }
}

#[async_trait]
impl AIProvider for OpenAICompatibleProvider {
    fn set_request_recorder(&mut self, recorder: RequestRecorder) {
        self.request_recorder = Some(recorder);
    }

    async fn chat_completions(
        &self,
        messages: &[ToolChatMessage],
        tools: &[ToolDefinition],
    ) -> anyhow::Result<ToolChatResponse> {
        let body = self.extra_body.merge(build_openai_compatible_request(
            &self.model,
            self.max_tokens,
            &self.reasoning_effort,
            messages,
            tools,
        ))?;
        trace!(
            provider = "openai_compatible",
            model = %self.model,
            request = %serde_json::to_string_pretty(&body)
                .unwrap_or_else(|error| format!("序列化请求失败: {}", error)),
            "AI Provider 完整请求"
        );
        let mut measurement = RequestMeasurement::begin(
            self.request_recorder.as_ref(),
            "openai_compatible",
            &self.model,
            "chat",
        );
        let result = async {
            let raw_response = self
                .send_request(&body, "OpenAI Compatible API", &mut measurement)
                .await?;
            parse_openai_compatible_chat_response_value(raw_response)
        }
        .await;
        measurement.complete(result.is_ok());
        result
    }

    async fn describe_image(&self, image_data_url: &str, prompt: &str) -> anyhow::Result<String> {
        let body = self.extra_body.merge(OpenAICompatibleVisionRequest {
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
            max_tokens: self.max_tokens,
            reasoning_effort: VISION_REASONING_EFFORT,
        })?;
        debug!(
            provider = "openai_compatible",
            model = %self.model,
            prompt,
            image_data_url_bytes = image_data_url.len(),
            "视觉 Provider 请求"
        );

        let mut measurement = RequestMeasurement::begin(
            self.request_recorder.as_ref(),
            "openai_compatible",
            &self.model,
            "vision",
        );
        let result = async {
            let raw_response = self
                .send_request(&body, "OpenAI Compatible 视觉 API", &mut measurement)
                .await?;
            let parsed =
                parse_openai_compatible_response(&raw_response, "OpenAI Compatible 视觉响应")?;
            first_choice(&parsed, "OpenAI Compatible 视觉响应")?
                .message
                .content
                .clone()
                .ok_or_else(|| anyhow!("OpenAI Compatible 视觉响应内容为空"))
        }
        .await;
        measurement.complete(result.is_ok());
        result
    }

    async fn web_search(&self, question: &str) -> anyhow::Result<String> {
        let body = self.extra_body.merge(OpenAICompatibleWebSearchRequest {
            model: &self.model,
            messages: vec![OpenAICompatibleToolMessage::User {
                content: chat::UserContent::Text(question),
            }],
            web_search_options: OpenAICompatibleWebSearchOptions {
                search_context_size: WEB_SEARCH_CONTEXT_SIZE,
            },
        })?;
        trace!(
            provider = "openai_compatible",
            model = %self.model,
            request = %serde_json::to_string_pretty(&body)
                .unwrap_or_else(|error| format!("序列化请求失败: {}", error)),
            "联网搜索 Provider 完整请求"
        );

        let mut measurement = RequestMeasurement::begin(
            self.request_recorder.as_ref(),
            "openai_compatible",
            &self.model,
            "web_search",
        );
        let result = async {
            let raw_response = self
                .send_request(&body, "OpenAI Compatible 联网搜索 API", &mut measurement)
                .await?;
            let parsed =
                parse_openai_compatible_response(&raw_response, "OpenAI Compatible 联网搜索响应")?;
            let choice = first_choice(&parsed, "OpenAI Compatible 联网搜索响应")?;
            choice
                .message
                .content
                .as_deref()
                .map(str::trim)
                .filter(|content| !content.is_empty())
                .map(str::to_string)
                .ok_or_else(|| anyhow!("OpenAI Compatible 联网搜索响应内容为空"))
        }
        .await;
        measurement.complete(result.is_ok());
        result
    }
}

fn parse_openai_compatible_chat_response_value(
    raw_response: Value,
) -> anyhow::Result<ToolChatResponse> {
    let response_name = "OpenAI Compatible 响应";
    let parsed = parse_openai_compatible_response(&raw_response, response_name)?;
    let first_choice = first_choice(&parsed, response_name)?;
    let content = first_choice.message.content.clone();
    let reasoning_content = first_choice.message.reasoning_content.clone();
    let finish_reason = first_choice.finish_reason.clone();
    let mut tool_calls = Vec::new();
    for call in first_choice
        .message
        .tool_calls
        .as_deref()
        .unwrap_or_default()
    {
        if call.call_type != "function" {
            bail!("{}包含不支持的工具类型：{}", response_name, call.call_type);
        }
        tool_calls.push(ToolCall {
            id: call.id.clone(),
            name: call.function.name.clone(),
            arguments: call.function.arguments.clone(),
        });
    }
    if content.is_none() && tool_calls.is_empty() && finish_reason.as_deref() != Some("stop") {
        bail!(
            "{}既没有文本内容，也没有 tool_calls，且 finish_reason 不是 stop（当前：{:?}）",
            response_name,
            finish_reason
        );
    }

    let usage = ChatUsage::from_chat_completions(&raw_response);

    Ok(ToolChatResponse {
        content,
        reasoning: ReasoningState {
            display_text: reasoning_content.clone(),
            replay: reasoning_content.map(ReasoningPayload::Text),
        },
        tool_calls,
        finish_reason,
        id: parsed.id,
        model: parsed.model,
        usage,
        raw_response,
    })
}

fn parse_openai_compatible_response(
    raw_response: &Value,
    response_name: &str,
) -> anyhow::Result<OpenAICompatibleChatResponse> {
    OpenAICompatibleChatResponse::deserialize(raw_response)
        .map_err(|error| anyhow!("解析{}失败：{}", response_name, error))
}

fn first_choice<'a>(
    response: &'a OpenAICompatibleChatResponse,
    response_name: &str,
) -> anyhow::Result<&'a OpenAICompatibleChoice> {
    let Some(choices) = response.choices.as_ref() else {
        return Err(anyhow!(
            "{}缺少 choices 数组，可能是上游返回了 choices=null 或错误体。",
            response_name
        ));
    };

    choices
        .first()
        .ok_or_else(|| anyhow!("{}的 choices 为空。", response_name))
}
