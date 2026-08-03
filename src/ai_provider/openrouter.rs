use crate::ai_provider::{AIProvider, ChatUsage, ToolChatMessage, ToolChatResponse};
use crate::tools::{ToolCall, ToolDefinition};
use anyhow::{anyhow, bail};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 视觉识别只做图片转写，固定使用 OpenRouter 支持的最低推理强度。
const VISION_REASONING_EFFORT: &str = "low";

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
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenRouterToolDefinition<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
}

#[derive(Serialize)]
struct OpenRouterReasoning<'a> {
    effort: &'a str,
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
        content: &'a str,
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
    max_completion_tokens: i32,
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
    max_tokens: i32,
    reasoning_effort: &'a str,
    messages: &'a [ToolChatMessage],
    tools: &'a [ToolDefinition],
) -> OpenRouterChatRequest<'a> {
    let messages = messages
        .iter()
        .map(|message| match message {
            ToolChatMessage::System { content } => OpenRouterChatMessage::System {
                content: content.as_str(),
            },
            ToolChatMessage::User { content } => OpenRouterChatMessage::User {
                content: content.as_str(),
            },
            ToolChatMessage::Assistant {
                content,
                reasoning_content,
                reasoning_details,
                tool_calls,
            } => OpenRouterChatMessage::Assistant {
                content: content.as_deref(),
                reasoning: if reasoning_details.is_none() {
                    reasoning_content.as_deref()
                } else {
                    None
                },
                reasoning_details: reasoning_details.as_ref(),
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
        messages,
        max_completion_tokens: max_tokens,
        reasoning: OpenRouterReasoning {
            effort: reasoning_effort,
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
    async fn chat_completions(
        &self,
        messages: &[ToolChatMessage],
        tools: &[ToolDefinition],
    ) -> anyhow::Result<ToolChatResponse> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = build_openrouter_chat_request(
            &self.model,
            self.max_tokens,
            &self.reasoning_effort,
            messages,
            tools,
        );
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
                "OpenRouter API 调用失败，状态码 {}：{}",
                status,
                error_text
            ));
        }

        let response_text = resp.text().await?;
        parse_openrouter_chat_response(&response_text)
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
                        },
                    },
                ],
            }],
            max_completion_tokens: self.max_tokens,
            reasoning: OpenRouterReasoning {
                effort: VISION_REASONING_EFFORT,
                summary: "auto",
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
                "OpenRouter 视觉 API 调用失败，状态码 {}：{}",
                status,
                error_text
            ));
        }

        let response_text = resp.text().await?;
        let parsed: OpenRouterChatResponse = serde_json::from_str(&response_text).map_err(|e| {
            anyhow!(
                "解析 OpenRouter 视觉响应失败：{}\n原始响应：\n{}",
                e,
                response_text
            )
        })?;
        parsed
            .choices
            .get(0)
            .and_then(|c| c.message.content.clone())
            .ok_or_else(|| anyhow!("OpenRouter 视觉响应内容为空"))
    }
}

fn parse_openrouter_chat_response(response_text: &str) -> anyhow::Result<ToolChatResponse> {
    let raw_response: Value = serde_json::from_str(response_text).map_err(|error| {
        anyhow!(
            "解析 OpenRouter 原始 JSON 失败：{}\n原始响应：\n{}",
            error,
            response_text
        )
    })?;
    let parsed: OpenRouterChatResponse = serde_json::from_str(response_text).map_err(|error| {
        anyhow!(
            "解析 OpenRouter 响应失败：{}\n原始响应：\n{}",
            error,
            response_text
        )
    })?;
    let first_choice = parsed.choices.first().ok_or_else(|| {
        anyhow!(
            "OpenRouter 响应 choices 为空\n原始响应：\n{}",
            response_text
        )
    })?;
    let content = first_choice.message.content.clone();
    let reasoning_details = first_choice.message.reasoning_details.clone();
    let reasoning_content = first_choice
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
            bail!(
                "OpenRouter 响应包含不支持的工具类型：{}\n原始响应：\n{}",
                call.call_type,
                response_text
            );
        }
        tool_calls.push(ToolCall {
            id: call.id.clone(),
            name: call.function.name.clone(),
            arguments: call.function.arguments.clone(),
        });
    }
    if content.is_none() && tool_calls.is_empty() {
        bail!(
            "OpenRouter 响应既没有文本内容，也没有 tool_calls\n原始响应：\n{}",
            response_text
        );
    }

    let finish_reason = first_choice.finish_reason.clone();
    let usage = parsed.usage.map(|usage| ChatUsage {
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        total_tokens: usage.total_tokens,
    });

    Ok(ToolChatResponse {
        content,
        reasoning_content,
        reasoning_details,
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
    use serde_json::json;

    use super::{build_openrouter_chat_request, parse_openrouter_chat_response};
    use crate::ai_provider::{ToolChatMessage, ToolChatResponse};
    use crate::tools::{ToolCall, ToolDefinition};

    #[test]
    fn serializes_tools_and_tool_history_in_openrouter_format() {
        let tools = vec![ToolDefinition {
            name: "web_search",
            description: "搜索互联网",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }];
        let reasoning_details = json!([{
            "type": "reasoning.text",
            "text": "需要实时天气"
        }]);
        let messages = vec![
            ToolChatMessage::User {
                content: "查询天气".to_string(),
            },
            ToolChatMessage::Assistant {
                content: Some(String::new()),
                reasoning_content: Some("需要实时天气".to_string()),
                reasoning_details: Some(reasoning_details.clone()),
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "web_search".to_string(),
                    arguments: r#"{"query":"香港天气"}"#.to_string(),
                }],
            },
            ToolChatMessage::Tool {
                tool_call_id: "call_1".to_string(),
                content: "晴，28 摄氏度".to_string(),
            },
        ];

        let body = build_openrouter_chat_request("test-model", 1024, "medium", &messages, &tools);
        let value = serde_json::to_value(body).unwrap();

        assert_eq!(value["tool_choice"], "auto");
        assert_eq!(value["reasoning"]["summary"], "auto");
        assert_eq!(value["tools"][0]["type"], "function");
        assert_eq!(value["tools"][0]["function"]["name"], "web_search");
        assert_eq!(value["messages"][1]["role"], "assistant");
        assert_eq!(value["messages"][1]["reasoning_details"], reasoning_details);
        assert!(value["messages"][1].get("reasoning").is_none());
        assert_eq!(
            value["messages"][1]["tool_calls"][0]["function"]["arguments"],
            r#"{"query":"香港天气"}"#
        );
        assert_eq!(value["messages"][2]["role"], "tool");
        assert_eq!(value["messages"][2]["tool_call_id"], "call_1");
    }

    #[test]
    fn omits_tool_fields_for_plain_text_request() {
        let messages = vec![ToolChatMessage::User {
            content: "需要回复吗".to_string(),
        }];

        let body = build_openrouter_chat_request("test-model", 128, "low", &messages, &[]);
        let value = serde_json::to_value(body).unwrap();

        assert!(value.get("tools").is_none());
        assert!(value.get("tool_choice").is_none());
        assert_eq!(value["reasoning"]["summary"], "auto");
    }

    #[test]
    fn parses_tool_calls_and_preserves_reasoning_details() {
        let response_text = r#"{
            "id": "generation_1",
            "model": "google/gemini-3-flash-preview",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_details": [{
                        "type": "reasoning.text",
                        "text": "需要调用搜索工具"
                    }],
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "web_search",
                            "arguments": "{\"query\":\"香港天气\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        }"#;

        let response = parse_openrouter_chat_response(response_text).unwrap();

        assert_eq!(response.content, None);
        assert_eq!(
            response.reasoning_content.as_deref(),
            Some("需要调用搜索工具")
        );
        assert!(response.reasoning_details.is_some());
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_1");
        assert_eq!(response.tool_calls[0].name, "web_search");
        assert_eq!(response.tool_calls[0].arguments, r#"{"query":"香港天气"}"#);
        assert!(matches!(
            ToolChatResponse::assistant_message(&response),
            ToolChatMessage::Assistant {
                reasoning_details: Some(_),
                tool_calls,
                ..
            } if tool_calls == response.tool_calls
        ));
    }
}
