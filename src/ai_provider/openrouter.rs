use crate::ai_provider::{
    AIProvider, ChatUsage, ReasoningPayload, ReasoningState, ToolChatContentPart, ToolChatMessage,
    ToolChatResponse, ToolChatUserContent,
};
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
        }
    }

    async fn send_chat_completions(
        &self,
        messages: &[ToolChatMessage],
        tools: &[ToolDefinition],
        session_id: Option<&str>,
    ) -> anyhow::Result<ToolChatResponse> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = build_openrouter_chat_request(
            &self.model,
            self.max_tokens,
            &self.reasoning_effort,
            messages,
            tools,
            session_id,
        );
        trace!(
            provider = "openrouter",
            model = %self.model,
            request = %serde_json::to_string_pretty(&body)
                .unwrap_or_else(|error| format!("序列化请求失败: {}", error)),
            "AI Provider 完整请求"
        );
        let resp = self
            .http_client
            .post(url)
            .bearer_auth(&self.api_key)
            .header("HTTP-Referer", OPENROUTER_APP_URL)
            .header("X-OpenRouter-Title", OPENROUTER_APP_TITLE)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let error_text = resp.text().await.unwrap_or_default();
            trace!(provider = "openrouter", status = %status, response = %error_text, "AI Provider 原始错误响应");
            return Err(anyhow!("OpenRouter API 调用失败，状态码 {}", status));
        }

        let response_text = resp.text().await?;
        trace!(provider = "openrouter", response = %response_text, "AI Provider 原始响应");
        parse_openrouter_chat_response(&response_text)
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
    tools: Option<Vec<OpenRouterToolDefinition<'a>>>,
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
        content: OpenRouterUserContent<'a>,
    },
    Assistant {
        content: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_details: Option<&'a Value>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<OpenRouterRequestToolCall<'a>>,
    },
    Tool {
        tool_call_id: &'a str,
        content: &'a str,
    },
}

#[derive(Serialize)]
#[serde(untagged)]
enum OpenRouterUserContent<'a> {
    Text(&'a str),
    Parts(Vec<OpenRouterUserContentPart<'a>>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenRouterUserContentPart<'a> {
    Text { text: &'a str },
    ImageUrl { image_url: OpenRouterImageUrl<'a> },
}

#[derive(Serialize)]
struct OpenRouterToolDefinition<'a> {
    #[serde(rename = "type")]
    definition_type: &'static str,
    function: OpenRouterFunctionDefinition<'a>,
}

#[derive(Serialize)]
struct OpenRouterFunctionDefinition<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

#[derive(Serialize)]
struct OpenRouterRequestToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    call_type: &'static str,
    function: OpenRouterRequestFunctionCall<'a>,
}

#[derive(Serialize)]
struct OpenRouterRequestFunctionCall<'a> {
    name: &'a str,
    arguments: &'a str,
}

/// OpenRouter 视觉描述请求结构
#[derive(Serialize)]
struct OpenRouterVisionRequest<'a> {
    model: &'a str,
    messages: Vec<OpenRouterVisionMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<i32>,
    reasoning: OpenRouterReasoning<'a>,
}

#[derive(Serialize)]
struct OpenRouterVisionMessage<'a> {
    role: &'a str,
    content: Vec<OpenRouterVisionContent<'a>>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenRouterVisionContent<'a> {
    Text { text: &'a str },
    ImageUrl { image_url: OpenRouterImageUrl<'a> },
}

#[derive(Serialize)]
struct OpenRouterImageUrl<'a> {
    url: &'a str,
    detail: &'static str,
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
    tool_calls: Option<Vec<OpenRouterResponseToolCall>>,
}

#[derive(Deserialize)]
struct OpenRouterResponseToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: OpenRouterResponseFunctionCall,
}

#[derive(Deserialize)]
struct OpenRouterResponseFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct OpenRouterUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
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
                content: openrouter_user_content(content),
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
                    .map(|call| OpenRouterRequestToolCall {
                        id: call.id.as_str(),
                        call_type: "function",
                        function: OpenRouterRequestFunctionCall {
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
            .map(|tool| OpenRouterToolDefinition {
                definition_type: "function",
                function: OpenRouterFunctionDefinition {
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

fn openrouter_user_content(content: &ToolChatUserContent) -> OpenRouterUserContent<'_> {
    if let Some(text) = content.as_text() {
        OpenRouterUserContent::Text(text)
    } else {
        OpenRouterUserContent::Parts(
            content
                .parts()
                .iter()
                .map(|part| match part {
                    ToolChatContentPart::Text { text } => OpenRouterUserContentPart::Text { text },
                    ToolChatContentPart::Image { data_url } => {
                        OpenRouterUserContentPart::ImageUrl {
                            image_url: OpenRouterImageUrl {
                                url: data_url,
                                detail: "high",
                            },
                        }
                    }
                })
                .collect(),
        )
    }
}

#[async_trait]
impl AIProvider for OpenRouterProvider {
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
        let body = OpenRouterVisionRequest {
            model: &self.model,
            messages: vec![OpenRouterVisionMessage {
                role: "user",
                content: vec![
                    OpenRouterVisionContent::Text { text: prompt },
                    OpenRouterVisionContent::ImageUrl {
                        image_url: OpenRouterImageUrl {
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
        };
        debug!(
            provider = "openrouter",
            model = %self.model,
            prompt,
            image_data_url_bytes = image_data_url.len(),
            "视觉 Provider 请求"
        );

        let resp = self
            .http_client
            .post(url)
            .bearer_auth(&self.api_key)
            .header("HTTP-Referer", OPENROUTER_APP_URL)
            .header("X-OpenRouter-Title", OPENROUTER_APP_TITLE)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let error_text = resp.text().await.unwrap_or_default();
            trace!(provider = "openrouter", status = %status, response = %error_text, "视觉 Provider 原始错误响应");
            return Err(anyhow!("OpenRouter 视觉 API 调用失败，状态码 {}", status));
        }

        let response_text = resp.text().await?;
        trace!(provider = "openrouter", response = %response_text, "视觉 Provider 原始响应");
        let parsed: OpenRouterChatResponse = serde_json::from_str(&response_text)
            .map_err(|e| anyhow!("解析 OpenRouter 视觉响应失败：{}", e))?;
        parsed
            .choices
            .get(0)
            .and_then(|c| c.message.content.clone())
            .ok_or_else(|| anyhow!("OpenRouter 视觉响应内容为空"))
    }
}

fn parse_openrouter_chat_response(response_text: &str) -> anyhow::Result<ToolChatResponse> {
    let raw_response: Value = serde_json::from_str(response_text)
        .map_err(|error| anyhow!("解析 OpenRouter 原始 JSON 失败：{}", error))?;
    let parsed: OpenRouterChatResponse = serde_json::from_str(response_text)
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

    let usage = parsed.usage.map(|usage| ChatUsage {
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        total_tokens: usage.total_tokens,
    });

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

#[cfg(test)]
mod tests {
    use super::*;

    // 验证结构化推理会原样回传给 OpenRouter。
    #[test]
    fn structured_reasoning_is_replayed_without_conversion() {
        let response = parse_openrouter_chat_response(
            r#"{
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "reasoning_details": [{
                            "type": "reasoning.text",
                            "text": "原始结构化推理"
                        }],
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "test_tool", "arguments": "{}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }"#,
        )
        .unwrap();

        assert_eq!(
            response.reasoning.display_text.as_deref(),
            Some("原始结构化推理")
        );
        let expected_details =
            response.raw_response["choices"][0]["message"]["reasoning_details"].clone();
        let messages = vec![response.assistant_message()];
        let request = build_openrouter_chat_request(
            "test",
            Some(100),
            "medium",
            &messages,
            &[],
            Some("rolava:chat:test"),
        );
        let request_json = serde_json::to_value(request).unwrap();

        assert_eq!(request_json["session_id"], "rolava:chat:test");
        assert_eq!(
            request_json["messages"][0]["reasoning_details"],
            expected_details
        );
        assert!(request_json["messages"][0].get("reasoning").is_none());
    }

    // 验证图文消息使用高精度图片参数。
    #[test]
    fn multimodal_user_message_uses_high_detail_image_url() {
        let messages = vec![ToolChatMessage::User {
            content: ToolChatUserContent::from_parts(vec![
                ToolChatContentPart::Text {
                    text: "看看这张图".to_string(),
                },
                ToolChatContentPart::Image {
                    data_url: "data:image/png;base64,abc".to_string(),
                },
            ]),
        }];

        let request =
            build_openrouter_chat_request("test", Some(100), "medium", &messages, &[], None);
        let request_json = serde_json::to_value(request).unwrap();

        assert_eq!(request_json["messages"][0]["content"][0]["type"], "text");
        assert_eq!(
            request_json["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,abc"
        );
        assert_eq!(
            request_json["messages"][0]["content"][1]["image_url"]["detail"],
            "high"
        );
    }

    #[test]
    fn automatic_defaults_keep_summary_without_forcing_limits() {
        let messages = vec![ToolChatMessage::User {
            content: ToolChatUserContent::text("test"),
        }];
        let request = build_openrouter_chat_request("test", None, "auto", &messages, &[], None);
        let request_json = serde_json::to_value(request).unwrap();

        assert!(request_json.get("max_completion_tokens").is_none());
        assert!(request_json["reasoning"].get("effort").is_none());
        assert_eq!(request_json["reasoning"]["summary"], "auto");
    }
}
