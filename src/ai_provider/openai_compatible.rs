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
        content: OpenAICompatibleUserContent<'a>,
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
#[serde(untagged)]
enum OpenAICompatibleUserContent<'a> {
    Text(&'a str),
    Parts(Vec<OpenAICompatibleUserContentPart<'a>>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAICompatibleUserContentPart<'a> {
    Text {
        text: &'a str,
    },
    ImageUrl {
        image_url: OpenAICompatibleImageUrl<'a>,
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
    detail: &'static str,
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
                content: openai_user_content(content),
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

fn openai_user_content(content: &ToolChatUserContent) -> OpenAICompatibleUserContent<'_> {
    if let Some(text) = content.as_text() {
        OpenAICompatibleUserContent::Text(text)
    } else {
        OpenAICompatibleUserContent::Parts(
            content
                .parts()
                .iter()
                .map(|part| match part {
                    ToolChatContentPart::Text { text } => {
                        OpenAICompatibleUserContentPart::Text { text }
                    }
                    ToolChatContentPart::Image { data_url } => {
                        OpenAICompatibleUserContentPart::ImageUrl {
                            image_url: OpenAICompatibleImageUrl {
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
        trace!(
            provider = "openai_compatible",
            model = %self.model,
            request = %serde_json::to_string_pretty(&body)
                .unwrap_or_else(|error| format!("序列化请求失败: {}", error)),
            "AI Provider 完整请求"
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
            trace!(provider = "openai_compatible", status = %status, response = %error_text, "AI Provider 原始错误响应");
            return Err(anyhow!("OpenAI Compatible API 调用失败，状态码 {}", status));
        }

        let response_text = resp.text().await?;
        trace!(provider = "openai_compatible", response = %response_text, "AI Provider 原始响应");
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
                            detail: "high",
                        },
                    },
                ],
            }],
            max_tokens: self.max_tokens,
            reasoning_effort: VISION_REASONING_EFFORT,
        };
        debug!(
            provider = "openai_compatible",
            model = %self.model,
            prompt,
            image_data_url_bytes = image_data_url.len(),
            "视觉 Provider 请求"
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
            trace!(provider = "openai_compatible", status = %status, response = %error_text, "视觉 Provider 原始错误响应");
            return Err(anyhow!(
                "OpenAI Compatible 视觉 API 调用失败，状态码 {}",
                status
            ));
        }

        let response_text = resp.text().await?;
        trace!(provider = "openai_compatible", response = %response_text, "视觉 Provider 原始响应");
        let parsed =
            parse_openai_compatible_response(&response_text, "OpenAI Compatible 视觉响应")?;
        first_choice(&parsed, &response_text, "OpenAI Compatible 视觉响应")?
            .message
            .content
            .clone()
            .ok_or_else(|| anyhow!("OpenAI Compatible 视觉响应内容为空"))
    }

    async fn web_search(&self, question: &str) -> anyhow::Result<String> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = OpenAICompatibleWebSearchRequest {
            model: &self.model,
            messages: vec![OpenAICompatibleToolMessage::User {
                content: OpenAICompatibleUserContent::Text(question),
            }],
            web_search_options: OpenAICompatibleWebSearchOptions {
                search_context_size: WEB_SEARCH_CONTEXT_SIZE,
            },
        };
        trace!(
            provider = "openai_compatible",
            model = %self.model,
            request = %serde_json::to_string_pretty(&body)
                .unwrap_or_else(|error| format!("序列化请求失败: {}", error)),
            "联网搜索 Provider 完整请求"
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
            trace!(provider = "openai_compatible", status = %status, response = %error_text, "联网搜索 Provider 原始错误响应");
            return Err(anyhow!(
                "OpenAI Compatible 联网搜索 API 调用失败，状态码 {}",
                status
            ));
        }

        let response_text = resp.text().await?;
        trace!(provider = "openai_compatible", response = %response_text, "联网搜索 Provider 原始响应");
        let parsed =
            parse_openai_compatible_response(&response_text, "OpenAI Compatible 联网搜索响应")?;
        let choice = first_choice(&parsed, &response_text, "OpenAI Compatible 联网搜索响应")?;
        choice
            .message
            .content
            .as_deref()
            .map(str::trim)
            .filter(|content| !content.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("OpenAI Compatible 联网搜索响应内容为空"))
    }
}

fn parse_openai_compatible_chat_response(response_text: &str) -> anyhow::Result<ToolChatResponse> {
    let response_name = "OpenAI Compatible 响应";
    let parsed = parse_openai_compatible_response(response_text, response_name)?;
    let raw_response: Value = serde_json::from_str(response_text)
        .map_err(|error| anyhow!("解析{}原始 JSON 失败：{}", response_name, error))?;
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

    let usage = parsed.usage.map(|usage| ChatUsage {
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        total_tokens: usage.total_tokens,
    });

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
    response_text: &str,
    response_name: &str,
) -> anyhow::Result<OpenAICompatibleChatResponse> {
    serde_json::from_str(response_text)
        .map_err(|error| anyhow!("解析{}失败：{}", response_name, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_reasoning_is_replayed_as_openai_reasoning_content() {
        let response = parse_openai_compatible_chat_response(
            r#"{
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "reasoning_content": "原始推理文本",
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
            Some("原始推理文本")
        );
        let messages = vec![response.assistant_message()];
        let request = build_openai_compatible_request("test", 100, "medium", &messages, &[]);
        let request_json = serde_json::to_value(request).unwrap();

        assert_eq!(
            request_json["messages"][0]["reasoning_content"],
            "原始推理文本"
        );
    }

    #[test]
    fn empty_stop_response_is_valid_no_action() {
        let response = parse_openai_compatible_chat_response(
            r#"{
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": []
                    },
                    "finish_reason": "stop"
                }]
            }"#,
        )
        .unwrap();

        assert_eq!(response.content, None);
        assert!(response.tool_calls.is_empty());
        assert_eq!(response.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn empty_non_stop_response_is_rejected() {
        let error = parse_openai_compatible_chat_response(
            r#"{
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": []
                    },
                    "finish_reason": "length"
                }]
            }"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("finish_reason 不是 stop"));
    }

    #[test]
    fn web_search_request_uses_official_medium_context_option() {
        let request = OpenAICompatibleWebSearchRequest {
            model: "gpt-5-search-api",
            messages: vec![OpenAICompatibleToolMessage::User {
                content: OpenAICompatibleUserContent::Text("今天有什么新闻？"),
            }],
            web_search_options: OpenAICompatibleWebSearchOptions {
                search_context_size: WEB_SEARCH_CONTEXT_SIZE,
            },
        };
        let request_json = serde_json::to_value(request).unwrap();

        assert_eq!(request_json["model"], "gpt-5-search-api");
        assert_eq!(request_json["messages"][0]["role"], "user");
        assert_eq!(
            request_json["web_search_options"]["search_context_size"],
            "medium"
        );
    }

    #[test]
    fn plain_text_user_message_stays_string() {
        let messages = vec![ToolChatMessage::User {
            content: ToolChatUserContent::text("纯文本"),
        }];

        let request = build_openai_compatible_request("test", 100, "medium", &messages, &[]);
        let request_json = serde_json::to_value(request).unwrap();

        assert_eq!(request_json["messages"][0]["content"], "纯文本");
    }

    #[test]
    fn multimodal_user_message_uses_high_detail_image_url() {
        let messages = vec![ToolChatMessage::User {
            content: ToolChatUserContent::from_parts(vec![
                ToolChatContentPart::Text {
                    text: "看看这张图".to_string(),
                },
                ToolChatContentPart::Image {
                    data_url: "data:image/jpeg;base64,abc".to_string(),
                },
            ]),
        }];

        let request = build_openai_compatible_request("test", 100, "medium", &messages, &[]);
        let request_json = serde_json::to_value(request).unwrap();

        assert_eq!(request_json["messages"][0]["content"][0]["type"], "text");
        assert_eq!(
            request_json["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/jpeg;base64,abc"
        );
        assert_eq!(
            request_json["messages"][0]["content"][1]["image_url"]["detail"],
            "high"
        );
    }
}

fn first_choice<'a>(
    response: &'a OpenAICompatibleChatResponse,
    _response_text: &str,
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
