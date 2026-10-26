use super::*;
use crate::ai_provider::{ReasoningPayload, ToolChatContentPart};
use crate::tools::ToolCall;
use anyhow::bail;
use serde_json::json;
use std::collections::HashMap;

pub(super) fn build_request(
    messages: &[ToolChatMessage],
    tools: &[ToolDefinition],
    max_tokens: Option<i32>,
) -> anyhow::Result<Value> {
    let (system, contents) = if let Ok(plain) = context_messages_without_tools(messages) {
        // 有原始 Gemini 载荷时必须走完整回传，不能降成纯文本。
        if messages.iter().any(|m| {
            matches!(
                m,
                ToolChatMessage::Assistant {
                    reasoning: Some(_),
                    ..
                }
            )
        }) {
            build_contents(messages)?
        } else {
            let (system, contents) = build_gemini_chat_contents(&plain);
            (
                serde_json::to_value(system)?,
                serde_json::to_value(contents)?,
            )
        }
    } else {
        build_contents(messages)?
    };
    let mut body = json!({"contents": contents});
    if !system.is_null() {
        body["systemInstruction"] = system;
    }
    if let Some(max_tokens) = max_tokens {
        body["generationConfig"] = json!({"maxOutputTokens": max_tokens});
    }
    if !tools.is_empty() {
        body["tools"] = json!([{"functionDeclarations": tools.iter().map(|tool| json!({
            "name": tool.name,
            "description": tool.description,
            "parametersJsonSchema": tool.parameters,
        })).collect::<Vec<_>>()}]);
        body["toolConfig"] = json!({"functionCallingConfig": {"mode": "AUTO"}});
    }
    Ok(body)
}

fn replay_content(reasoning: &Option<ReasoningPayload>) -> Option<&Value> {
    let Some(ReasoningPayload::Structured(payload)) = reasoning else {
        return None;
    };
    (payload.get("provider")?.as_str()? == "gemini").then(|| payload.get("content"))?
}

fn push_content(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    // 相邻工具结果合并为同一 user turn，原样保留每个 part 的边界。
    if let Some(last) = contents
        .last_mut()
        .filter(|last| role == "user" && last["role"] == role)
    {
        last["parts"].as_array_mut().unwrap().extend(parts);
    } else {
        contents.push(json!({"role": role, "parts": parts}));
    }
}

fn build_contents(messages: &[ToolChatMessage]) -> anyhow::Result<(Value, Value)> {
    let mut system = Vec::new();
    let mut contents = Vec::new();
    let mut pending = HashMap::new();
    let mut conversation_started = false;
    for message in messages {
        conversation_started |= !matches!(message, ToolChatMessage::System { .. });
        match message {
            ToolChatMessage::System { content } if !conversation_started => {
                system.push(content.as_str())
            }
            ToolChatMessage::System { content } => {
                push_content(&mut contents, "user", vec![json!({"text": content})]);
            }
            ToolChatMessage::User { content } => {
                let parts = content
                    .parts()
                    .iter()
                    .map(|part| match part {
                        ToolChatContentPart::Text { text } => Ok(json!({"text": text})),
                        ToolChatContentPart::Image { data_url } => {
                            Ok(json!({"inlineData": parse_data_url(data_url)?}))
                        }
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                push_content(&mut contents, "user", parts);
            }
            ToolChatMessage::Assistant {
                content,
                reasoning,
                tool_calls,
            } => {
                let replay = replay_content(reasoning);
                let mut parts = match replay {
                    Some(raw) => raw
                        .get("parts")
                        .and_then(Value::as_array)
                        .ok_or_else(|| anyhow!("Gemini 回传载荷缺少 parts"))?
                        .clone(),
                    None => content
                        .as_ref()
                        .map(|text| vec![json!({"text": text})])
                        .unwrap_or_default(),
                };
                let raw_calls = parts
                    .iter()
                    .filter_map(|part| part.get("functionCall"))
                    .cloned()
                    .collect::<Vec<_>>();
                if replay.is_some() && raw_calls.len() != tool_calls.len() {
                    bail!("Gemini 回传载荷与工具调用数量不一致");
                }
                for (index, call) in tool_calls.iter().enumerate() {
                    let args: Value = serde_json::from_str(&call.arguments)
                        .context("Gemini 工具参数不是有效 JSON")?;
                    if !args.is_object() {
                        bail!("Gemini 工具参数必须是 JSON 对象");
                    }
                    let mut function = json!({"name": call.name, "args": args});
                    if replay.is_some() {
                        function = raw_calls[index].clone();
                        if function["name"] != call.name {
                            bail!("Gemini 回传载荷与工具名称不一致");
                        }
                    } else {
                        // 非 Gemini 历史没有签名，不伪造签名；由模型按自身协议校验。
                        parts.push(json!({"functionCall": function}));
                    }
                    if pending.insert(call.id.clone(), function).is_some() {
                        bail!("Gemini 历史含重复的未完成工具调用：{}", call.id);
                    }
                }
                push_content(&mut contents, "model", parts);
            }
            ToolChatMessage::Tool {
                tool_call_id,
                content,
            } => {
                let function = pending
                    .remove(tool_call_id)
                    .ok_or_else(|| anyhow!("Gemini 工具结果缺少对应调用：{}", tool_call_id))?;
                let mut result = json!({"name": function["name"], "response": {"result": content}});
                if let Some(id) = function.get("id") {
                    result["id"] = id.clone();
                }
                push_content(
                    &mut contents,
                    "user",
                    vec![json!({"functionResponse": result})],
                );
            }
        }
    }
    if !pending.is_empty() {
        bail!("Gemini 历史存在缺少结果的工具调用");
    }
    let system = if system.is_empty() {
        Value::Null
    } else {
        json!({"parts": [{"text": system.join("\n\n")}]})
    };
    Ok((system, json!(contents)))
}

pub(super) fn validate_response(raw: &Value) -> anyhow::Result<()> {
    if let Some(error) = raw.get("error") {
        bail!("Gemini 响应错误：{}", error);
    }
    let candidate = raw
        .pointer("/candidates/0")
        .ok_or_else(|| anyhow!("Gemini 响应缺少候选内容：{}", raw))?;
    // TODO: 长输出续传尚未实现，CONTINUATION 在此按未完成响应处理。
    // 续传需保持原始请求不变，携带候选内容的 continuationToken，直到结束后合并各段 parts。
    if candidate.get("finishReason").and_then(Value::as_str) != Some("STOP") {
        bail!("Gemini 响应未正常完成：{}", raw);
    }
    Ok(())
}

pub(super) fn parse_response_value(raw: Value, model: &str) -> anyhow::Result<ToolChatResponse> {
    validate_response(&raw)?;
    let candidate = &raw["candidates"][0];
    let parts = candidate
        .pointer("/content/parts")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Gemini 响应缺少 parts"))?;
    let mut answer = String::new();
    let mut thoughts = String::new();
    let mut calls = Vec::new();
    let request_id = format!("gemini_{:032x}", rand::random::<u128>());
    for part in parts {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                thoughts.push_str(text);
            } else {
                answer.push_str(text);
            }
        }
        if let Some(function) = part.get("functionCall") {
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| anyhow!("Gemini functionCall 缺少名称"))?;
            let args = function.get("args").cloned().unwrap_or_else(|| json!({}));
            if !args.is_object() {
                bail!("Gemini functionCall.args 必须是对象");
            }
            // 内部 ID 独立于上游可选 ID，避免无 ID 或跨轮重复 ID 导致历史冲突。
            calls.push(ToolCall {
                id: format!("{}_{}", request_id, calls.len()),
                name: name.to_string(),
                arguments: serde_json::to_string(&args)?,
            });
        }
    }
    if answer.trim().is_empty() && calls.is_empty() {
        bail!("Gemini 响应内容为空");
    }
    let usage = ChatUsage::from_gemini(&raw);
    Ok(ToolChatResponse {
        content: (!answer.trim().is_empty()).then_some(answer),
        reasoning: ReasoningState {
            display_text: (!thoughts.trim().is_empty()).then_some(thoughts),
            replay: Some(ReasoningPayload::Structured(json!({
                "provider": "gemini", "content": candidate["content"],
            }))),
        },
        tool_calls: calls,
        finish_reason: Some("STOP".to_string()),
        id: raw
            .get("responseId")
            .and_then(Value::as_str)
            .map(str::to_string),
        model: Some(
            raw.get("modelVersion")
                .and_then(Value::as_str)
                .unwrap_or(model)
                .to_string(),
        ),
        usage,
        raw_response: raw,
    })
}
