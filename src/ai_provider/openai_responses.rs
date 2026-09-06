use crate::ai_provider::{
    AIProvider, ChatUsage, ReasoningPayload, ReasoningState, ToolChatContentPart, ToolChatMessage,
    ToolChatResponse, ToolChatUserContent,
};
use crate::tools::{ToolCall, ToolDefinition};
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;
use serde_json::{json, Value};
use tracing::{debug, trace};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const RESPONSES_REPLAY_PROVIDER: &str = "openai_responses";

/// OpenAI Responses API Provider。由应用管理完整上下文，不使用服务端会话存储。
pub struct OpenAIResponsesProvider {
    http_client: Client,
    api_key: String,
    base_url: String,
    model: String,
    max_tokens: Option<i32>,
    reasoning_effort: String,
}

impl OpenAIResponsesProvider {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        max_tokens: Option<i32>,
        reasoning_effort: impl Into<String>,
    ) -> Self {
        let base_url = base_url.into();
        let base_url = if base_url.trim().is_empty() {
            DEFAULT_BASE_URL.to_string()
        } else {
            base_url.trim().trim_end_matches('/').to_string()
        };
        Self {
            http_client: Client::new(),
            api_key: api_key.into(),
            base_url,
            model: model.into(),
            max_tokens,
            reasoning_effort: reasoning_effort.into(),
        }
    }

    async fn send_request(
        &self,
        body: &OpenAIResponsesRequest<'_>,
        operation: &'static str,
    ) -> anyhow::Result<ToolChatResponse> {
        trace!(
            provider = "openai_responses",
            model = %self.model,
            request = %serde_json::to_string_pretty(body)
                .unwrap_or_else(|error| format!("序列化请求失败: {}", error)),
            "{}完整请求",
            operation
        );
        let response = self
            .http_client
            .post(format!("{}/responses", self.base_url))
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await?;
        let status = response.status();
        let response_text = response.text().await?;
        trace!(
            provider = "openai_responses",
            status = %status,
            response = %response_text,
            "{}原始响应",
            operation
        );
        if !status.is_success() {
            let detail = serde_json::from_str::<Value>(&response_text)
                .ok()
                .and_then(|body| {
                    body.pointer("/error/message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| response_text.chars().take(1000).collect());
            bail!(
                "OpenAI Responses API 调用失败，状态码 {}：{}",
                status,
                detail
            );
        }
        parse_openai_responses_response(&response_text)
    }

    async fn send_chat_completions(
        &self,
        messages: &[ToolChatMessage],
        tools: &[ToolDefinition],
        session_id: Option<&str>,
    ) -> anyhow::Result<ToolChatResponse> {
        let body = build_openai_responses_request(
            &self.model,
            self.max_tokens,
            &self.reasoning_effort,
            messages,
            tools,
            session_id,
        );
        self.send_request(&body, "AI Provider ").await
    }
}

#[derive(Serialize)]
struct OpenAIResponsesRequest<'a> {
    model: &'a str,
    input: Vec<Value>,
    store: bool,
    include: [&'static str; 1],
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<OpenAIResponsesReasoning<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAIResponsesTool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
}

#[derive(Serialize)]
struct OpenAIResponsesReasoning<'a> {
    effort: &'a str,
    summary: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAIResponsesTool<'a> {
    WebSearch {},
    Function {
        name: &'a str,
        description: &'a str,
        parameters: &'a Value,
        strict: bool,
    },
}

fn build_openai_responses_request<'a>(
    model: &'a str,
    max_tokens: Option<i32>,
    reasoning_effort: &'a str,
    messages: &'a [ToolChatMessage],
    tools: &'a [ToolDefinition],
    session_id: Option<&'a str>,
) -> OpenAIResponsesRequest<'a> {
    let tools = (!tools.is_empty()).then(|| {
        tools
            .iter()
            .map(|tool| OpenAIResponsesTool::Function {
                name: tool.name,
                description: tool.description,
                parameters: &tool.parameters,
                strict: false,
            })
            .collect()
    });
    let tool_choice = tools.as_ref().map(|_| "auto");
    OpenAIResponsesRequest {
        model,
        input: responses_input(messages),
        store: false,
        // 无服务端状态时回传加密 reasoning item，供本轮工具循环原样续接。
        include: ["reasoning.encrypted_content"],
        max_output_tokens: max_tokens,
        reasoning: (reasoning_effort != "auto").then_some(OpenAIResponsesReasoning {
            effort: reasoning_effort,
            summary: "auto",
        }),
        tools,
        tool_choice,
        prompt_cache_key: session_id,
    }
}

/// Responses 的 input 是异构 Item 数组；保留模型原始 output 才能维持工具调用顺序。
fn responses_input(messages: &[ToolChatMessage]) -> Vec<Value> {
    let mut input = Vec::new();
    for message in messages {
        match message {
            ToolChatMessage::System { content } => input.push(json!({
                "role": "system",
                "content": [{ "type": "input_text", "text": content }]
            })),
            ToolChatMessage::User { content } => input.push(json!({
                "role": "user",
                "content": responses_user_content(content)
            })),
            ToolChatMessage::Assistant {
                content,
                reasoning,
                tool_calls,
            } => {
                if let Some(items) = reasoning.as_ref().and_then(responses_replay_items) {
                    input.extend(items.iter().cloned());
                    continue;
                }
                if let Some(content) = content.as_deref().filter(|text| !text.is_empty()) {
                    input.push(json!({ "role": "assistant", "content": content }));
                }
                input.extend(tool_calls.iter().map(|call| {
                    json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments
                    })
                }));
            }
            ToolChatMessage::Tool {
                tool_call_id,
                content,
            } => input.push(json!({
                "type": "function_call_output",
                "call_id": tool_call_id,
                "output": content
            })),
        }
    }
    input
}

fn responses_user_content(content: &ToolChatUserContent) -> Vec<Value> {
    content
        .parts()
        .iter()
        .map(|part| match part {
            ToolChatContentPart::Text { text } => json!({ "type": "input_text", "text": text }),
            ToolChatContentPart::Image { data_url } => json!({
                "type": "input_image",
                "image_url": data_url,
                "detail": "high"
            }),
        })
        .collect()
}

fn responses_replay_items(reasoning: &ReasoningPayload) -> Option<&[Value]> {
    let ReasoningPayload::Structured(payload) = reasoning else {
        return None;
    };
    if payload.get("provider")?.as_str()? != RESPONSES_REPLAY_PROVIDER {
        return None;
    }
    payload.get("output_items")?.as_array().map(Vec::as_slice)
}

fn parse_openai_responses_response(response_text: &str) -> anyhow::Result<ToolChatResponse> {
    let raw_response: Value = serde_json::from_str(response_text)
        .map_err(|error| anyhow!("解析 OpenAI Responses 原始 JSON 失败：{}", error))?;
    let status = raw_response
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_string);
    if status.as_deref() == Some("failed") {
        let message = raw_response
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        bail!("OpenAI Responses 生成失败：{}", message);
    }
    if status.as_deref() != Some("completed") {
        let reason = raw_response
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
            .unwrap_or("未提供原因");
        bail!(
            "OpenAI Responses 生成未完成，状态：{:?}，原因：{}",
            status,
            reason
        );
    }

    let output = raw_response
        .get("output")
        .and_then(Value::as_array)
        .context("OpenAI Responses 响应缺少 output 数组")?;
    let mut text_parts = Vec::new();
    let mut reasoning_summaries = Vec::new();
    let mut tool_calls = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => collect_message_text(item, &mut text_parts),
            Some("function_call") => tool_calls.push(parse_function_call(item)?),
            Some("reasoning") => collect_reasoning_summary(item, &mut reasoning_summaries),
            _ => {}
        }
    }
    if text_parts.is_empty() {
        if let Some(text) = raw_response
            .get("output_text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            text_parts.push(text.to_string());
        }
    }
    let content = (!text_parts.is_empty()).then(|| text_parts.join("\n"));
    if content.is_none() && tool_calls.is_empty() {
        bail!(
            "OpenAI Responses 响应既没有文本内容，也没有 function_call（状态：{:?}）",
            status
        );
    }

    let finish_reason = Some(
        if tool_calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        }
        .to_string(),
    );
    let usage = raw_response.get("usage").map(|usage| ChatUsage {
        prompt_tokens: usage.get("input_tokens").and_then(Value::as_u64),
        completion_tokens: usage.get("output_tokens").and_then(Value::as_u64),
        total_tokens: usage.get("total_tokens").and_then(Value::as_u64),
    });
    let display_text = (!reasoning_summaries.is_empty()).then(|| reasoning_summaries.join("\n"));
    let replay = (!output.is_empty()).then(|| {
        ReasoningPayload::Structured(json!({
            "provider": RESPONSES_REPLAY_PROVIDER,
            "output_items": output
        }))
    });

    Ok(ToolChatResponse {
        content,
        reasoning: ReasoningState {
            display_text,
            replay,
        },
        tool_calls,
        finish_reason,
        id: raw_response
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string),
        model: raw_response
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
        usage,
        raw_response,
    })
}

fn collect_message_text(item: &Value, text_parts: &mut Vec<String>) {
    let Some(parts) = item.get("content").and_then(Value::as_array) else {
        return;
    };
    for part in parts {
        let text = match part.get("type").and_then(Value::as_str) {
            Some("output_text") => part.get("text").and_then(Value::as_str),
            Some("refusal") => part.get("refusal").and_then(Value::as_str),
            _ => None,
        };
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            text_parts.push(text.to_string());
        }
    }
}

fn collect_reasoning_summary(item: &Value, summaries: &mut Vec<String>) {
    let Some(parts) = item.get("summary").and_then(Value::as_array) else {
        return;
    };
    summaries.extend(parts.iter().filter_map(|part| {
        part.get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    }));
}

fn parse_function_call(item: &Value) -> anyhow::Result<ToolCall> {
    Ok(ToolCall {
        id: item
            .get("call_id")
            .and_then(Value::as_str)
            .context("OpenAI Responses function_call 缺少 call_id")?
            .to_string(),
        name: item
            .get("name")
            .and_then(Value::as_str)
            .context("OpenAI Responses function_call 缺少 name")?
            .to_string(),
        arguments: item
            .get("arguments")
            .and_then(Value::as_str)
            .context("OpenAI Responses function_call 缺少 arguments")?
            .to_string(),
    })
}

#[async_trait]
impl AIProvider for OpenAIResponsesProvider {
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
        let messages = [ToolChatMessage::User {
            content: ToolChatUserContent::from_parts(vec![
                ToolChatContentPart::Text {
                    text: prompt.to_string(),
                },
                ToolChatContentPart::Image {
                    data_url: image_data_url.to_string(),
                },
            ]),
        }];
        let body = build_openai_responses_request(
            &self.model,
            self.max_tokens,
            "auto",
            &messages,
            &[],
            None,
        );
        debug!(
            provider = "openai_responses",
            model = %self.model,
            prompt,
            image_data_url_bytes = image_data_url.len(),
            "视觉 Provider 请求"
        );
        self.send_request(&body, "视觉 Provider ")
            .await?
            .content
            .filter(|content| !content.trim().is_empty())
            .ok_or_else(|| anyhow!("OpenAI Responses 视觉响应内容为空"))
    }

    async fn web_search(&self, question: &str) -> anyhow::Result<String> {
        let messages = [ToolChatMessage::User {
            content: ToolChatUserContent::text(question),
        }];
        let mut body = build_openai_responses_request(
            &self.model,
            self.max_tokens,
            &self.reasoning_effort,
            &messages,
            &[],
            None,
        );
        body.tools = Some(vec![OpenAIResponsesTool::WebSearch {}]);
        body.tool_choice = Some("required");
        let response = self.send_request(&body, "联网搜索 Provider ").await?;
        let searched = response
            .raw_response
            .get("output")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("type").and_then(Value::as_str) == Some("web_search_call")
                        && item.get("status").and_then(Value::as_str) == Some("completed")
                })
            });
        if !searched {
            bail!("OpenAI Responses 未返回已完成的联网搜索调用");
        }
        response
            .content
            .filter(|content| !content.trim().is_empty())
            .ok_or_else(|| anyhow!("OpenAI Responses 联网搜索响应内容为空"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 即使携带可解析的正文和工具参数，未完成的响应也不能进入发送或执行流程。
    #[test]
    fn rejects_incomplete_output_before_exposing_text_or_tools() {
        let error = parse_openai_responses_response(
            r#"{
            "status":"incomplete",
            "incomplete_details":{"reason":"max_output_tokens"},
            "output":[
                {"type":"message","content":[{"type":"output_text","text":"未写完的回复"}]},
                {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"}
            ]
        }"#,
        )
        .err()
        .expect("未完成响应必须返回错误");
        assert!(error.to_string().contains("max_output_tokens"));
    }

    // 验证响应解析后，加密推理和工具调用能完整回传并衔接对应的工具结果。
    #[test]
    fn replays_exact_output_items_before_tool_results() {
        let response = parse_openai_responses_response(
            r#"{
                "id":"resp_1","status":"completed","model":"gpt-test",
                "output":[
                    {"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"abc"},
                    {"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{}"}
                ]
            }"#,
        )
        .unwrap();
        let input = responses_input(&[
            response.assistant_message(),
            ToolChatMessage::Tool {
                tool_call_id: "call_1".to_string(),
                content: "done".to_string(),
            },
        ]);

        assert_eq!(
            input[..2],
            response.raw_response["output"].as_array().unwrap()[..]
        );
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_1");
    }
}
