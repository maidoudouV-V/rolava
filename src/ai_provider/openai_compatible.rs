use crate::ai_provider::{AIProvider, ChatUsage, ToolChatMessage, ToolChatResponse};
use crate::tools::{ToolCall, ToolDefinition};
use anyhow::{anyhow, bail};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// OpenAI Compatible 聊天请求；tools 为空时省略工具相关字段。
#[derive(Serialize)]
struct OpenAICompatibleChatRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAICompatibleToolMessage<'a>>,
    max_tokens: i32,
    reasoning_effort: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAICompatibleToolDefinition<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
}

#[derive(Serialize)]
struct OpenAICompatibleToolDefinition<'a> {
    #[serde(rename = "type")]
    definition_type: &'static str,
    function: OpenAICompatibleFunctionDefinition<'a>,
}

#[derive(Serialize)]
struct OpenAICompatibleFunctionDefinition<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

#[derive(Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum OpenAICompatibleToolMessage<'a> {
    System {
        content: &'a str,
    },
    User {
        content: &'a str,
    },
    Assistant {
        content: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<&'a str>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<OpenAICompatibleRequestToolCall<'a>>,
    },
    Tool {
        tool_call_id: &'a str,
        content: &'a str,
    },
}

#[derive(Serialize)]
struct OpenAICompatibleRequestToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    call_type: &'static str,
    function: OpenAICompatibleRequestFunctionCall<'a>,
}

#[derive(Serialize)]
struct OpenAICompatibleRequestFunctionCall<'a> {
    name: &'a str,
    arguments: &'a str,
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
    choices: Option<Vec<OpenAICompatibleChoice>>,
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
    tool_calls: Option<Vec<OpenAICompatibleResponseToolCall>>,
}

#[derive(Deserialize)]
struct OpenAICompatibleResponseToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: OpenAICompatibleResponseFunctionCall,
}

#[derive(Deserialize)]
struct OpenAICompatibleResponseFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct OpenAICompatibleUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

fn build_openai_compatible_request<'a>(
    model: &'a str,
    max_tokens: i32,
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
                content: content.as_str(),
            },
            ToolChatMessage::Assistant {
                content,
                reasoning_content,
                tool_calls,
                ..
            } => OpenAICompatibleToolMessage::Assistant {
                content: content.as_deref(),
                reasoning_content: reasoning_content.as_deref(),
                tool_calls: tool_calls
                    .iter()
                    .map(|call| OpenAICompatibleRequestToolCall {
                        id: call.id.as_str(),
                        call_type: "function",
                        function: OpenAICompatibleRequestFunctionCall {
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
            .map(|tool| OpenAICompatibleToolDefinition {
                definition_type: "function",
                function: OpenAICompatibleFunctionDefinition {
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
        reasoning_effort,
        tools,
        tool_choice,
    }
}

#[async_trait]
impl AIProvider for OpenAICompatibleProvider {
    async fn chat_completions(
        &self,
        messages: &[ToolChatMessage],
        tools: &[ToolDefinition],
    ) -> anyhow::Result<ToolChatResponse> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = build_openai_compatible_request(
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
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let error_text = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "OpenAI Compatible API 调用失败，状态码 {}：{}",
                status,
                error_text
            ));
        }

        let response_text = resp.text().await?;
        parse_openai_compatible_chat_response(&response_text)
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
            return Err(anyhow!(
                "OpenAI Compatible 视觉 API 调用失败，状态码 {}：{}",
                status,
                error_text
            ));
        }

        let response_text = resp.text().await?;
        let parsed =
            parse_openai_compatible_response(&response_text, "OpenAI Compatible 视觉响应")?;
        first_choice(&parsed, &response_text, "OpenAI Compatible 视觉响应")?
            .message
            .content
            .clone()
            .ok_or_else(|| {
                anyhow!(
                    "OpenAI Compatible 视觉响应内容为空\n原始响应：\n{}",
                    response_text
                )
            })
    }
}

fn parse_openai_compatible_chat_response(response_text: &str) -> anyhow::Result<ToolChatResponse> {
    let response_name = "OpenAI Compatible 响应";
    let parsed = parse_openai_compatible_response(response_text, response_name)?;
    let raw_response: Value = serde_json::from_str(response_text).map_err(|error| {
        anyhow!(
            "解析{}原始 JSON 失败：{}\n原始响应：\n{}",
            response_name,
            error,
            response_text
        )
    })?;
    let first_choice = first_choice(&parsed, response_text, response_name)?;
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
            bail!(
                "{}包含不支持的工具类型：{}\n原始响应：\n{}",
                response_name,
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
            "{}既没有文本内容，也没有 tool_calls\n原始响应：\n{}",
            response_name,
            response_text
        );
    }

    let usage = parsed.usage.map(|usage| ChatUsage {
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        total_tokens: usage.total_tokens,
    });

    Ok(ToolChatResponse {
        content,
        reasoning_content,
        reasoning_details: None,
        tool_calls,
        finish_reason,
        id: parsed.id,
        model: parsed.model,
        usage,
        raw_response,
    })
}

fn parse_openai_compatible_response(
    response_text: &str,
    response_name: &str,
) -> anyhow::Result<OpenAICompatibleChatResponse> {
    serde_json::from_str(response_text).map_err(|e| {
        anyhow!(
            "解析{}失败：{}\n原始响应：\n{}",
            response_name,
            e,
            response_text
        )
    })
}

fn first_choice<'a>(
    response: &'a OpenAICompatibleChatResponse,
    response_text: &str,
    response_name: &str,
) -> anyhow::Result<&'a OpenAICompatibleChoice> {
    let Some(choices) = response.choices.as_ref() else {
        return Err(anyhow!(
            "{}缺少 choices 数组，可能是上游返回了 choices=null 或错误体。\n原始响应：\n{}",
            response_name,
            response_text
        ));
    };

    choices.get(0).ok_or_else(|| {
        anyhow!(
            "{}的 choices 为空。\n原始响应：\n{}",
            response_name,
            response_text
        )
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{build_openai_compatible_request, parse_openai_compatible_chat_response};
    use crate::ai_provider::{ToolChatMessage, ToolChatResponse};
    use crate::tools::{ToolCall, ToolDefinition};

    #[test]
    fn serializes_tools_and_tool_history_in_openai_format() {
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
        let messages = vec![
            ToolChatMessage::User {
                content: "查询天气".to_string(),
            },
            ToolChatMessage::Assistant {
                content: None,
                reasoning_content: Some("需要实时天气".to_string()),
                reasoning_details: None,
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

        let body = build_openai_compatible_request("test-model", 1024, "medium", &messages, &tools);
        let value = serde_json::to_value(body).unwrap();

        assert_eq!(value["tool_choice"], "auto");
        assert_eq!(value["tools"][0]["type"], "function");
        assert_eq!(value["tools"][0]["function"]["name"], "web_search");
        assert_eq!(value["messages"][1]["role"], "assistant");
        assert!(value["messages"][1]["content"].is_null());
        assert_eq!(value["messages"][1]["reasoning_content"], "需要实时天气");
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

        let body = build_openai_compatible_request("test-model", 128, "low", &messages, &[]);
        let value = serde_json::to_value(body).unwrap();

        assert!(value.get("tools").is_none());
        assert!(value.get("tool_choice").is_none());
        assert_eq!(value["messages"][0]["content"], "需要回复吗");
    }

    #[test]
    fn parses_plain_text_with_the_unified_response_parser() {
        let response_text = r#"{
            "id": "chatcmpl_text",
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "reply"
                },
                "finish_reason": "stop"
            }]
        }"#;

        let response = parse_openai_compatible_chat_response(response_text).unwrap();

        assert_eq!(response.content.as_deref(), Some("reply"));
        assert!(response.tool_calls.is_empty());
    }

    #[test]
    fn parses_tool_calls_when_assistant_content_is_null() {
        let response_text = r#"{
            "id": "chatcmpl_1",
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": "需要调用搜索工具",
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

        let response = parse_openai_compatible_chat_response(response_text).unwrap();

        assert_eq!(response.content, None);
        assert_eq!(
            response.reasoning_content.as_deref(),
            Some("需要调用搜索工具")
        );
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_1");
        assert_eq!(response.tool_calls[0].name, "web_search");
        assert_eq!(response.tool_calls[0].arguments, r#"{"query":"香港天气"}"#);
        assert!(matches!(
            ToolChatResponse::assistant_message(&response),
            ToolChatMessage::Assistant {
                reasoning_content: Some(reasoning_content),
                tool_calls,
                ..
            } if reasoning_content == "需要调用搜索工具" && tool_calls == response.tool_calls
        ));
    }
}
