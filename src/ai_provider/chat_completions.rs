use super::{ToolChatContentPart, ToolChatUserContent};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 读取一次响应 JSON，同时交给请求统计和供应商解析层。
pub(super) async fn read_response(
    response: reqwest::Response,
    measurement: &mut crate::request_statistics::RequestMeasurement,
    provider: &str,
    api_name: &str,
) -> anyhow::Result<Value> {
    let status = response.status();
    measurement.response_status(status.as_u16());
    let text = response
        .text()
        .await
        .with_context(|| format!("读取{}响应正文失败，HTTP {}", api_name, status))?;
    let raw = serde_json::from_str::<Value>(&text);
    if let Ok(raw) = &raw {
        measurement.response_json(raw);
    }
    tracing::trace!(provider, status = %status, response = %text, "AI Provider 原始响应");
    if !status.is_success() {
        anyhow::bail!("{} 调用失败，状态码 {}：{}", api_name, status, text);
    }
    raw.map_err(|error| anyhow::anyhow!("解析{}原始 JSON 失败：{}", api_name, error))
}

#[derive(Serialize)]
#[serde(untagged)]
pub(super) enum UserContent<'a> {
    Text(&'a str),
    Parts(Vec<UserContentPart<'a>>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum UserContentPart<'a> {
    Text { text: &'a str },
    ImageUrl { image_url: ImageUrl<'a> },
}

#[derive(Serialize)]
pub(super) struct ToolDefinition<'a> {
    #[serde(rename = "type")]
    pub(super) definition_type: &'static str,
    pub(super) function: FunctionDefinition<'a>,
}

#[derive(Serialize)]
pub(super) struct FunctionDefinition<'a> {
    pub(super) name: &'a str,
    pub(super) description: &'a str,
    pub(super) parameters: &'a Value,
}

#[derive(Serialize)]
pub(super) struct RequestToolCall<'a> {
    pub(super) id: &'a str,
    #[serde(rename = "type")]
    pub(super) call_type: &'static str,
    pub(super) function: RequestFunctionCall<'a>,
}

#[derive(Serialize)]
pub(super) struct RequestFunctionCall<'a> {
    pub(super) name: &'a str,
    pub(super) arguments: &'a str,
}

#[derive(Serialize)]
pub(super) struct VisionMessage<'a> {
    pub(super) role: &'a str,
    pub(super) content: Vec<VisionContent<'a>>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum VisionContent<'a> {
    Text { text: &'a str },
    ImageUrl { image_url: ImageUrl<'a> },
}

#[derive(Serialize)]
pub(super) struct ImageUrl<'a> {
    pub(super) url: &'a str,
    pub(super) detail: &'static str,
}

#[derive(Deserialize)]
pub(super) struct ResponseToolCall {
    pub(super) id: String,
    #[serde(rename = "type")]
    pub(super) call_type: String,
    pub(super) function: ResponseFunctionCall,
}

#[derive(Deserialize)]
pub(super) struct ResponseFunctionCall {
    pub(super) name: String,
    pub(super) arguments: String,
}

pub(super) fn user_content(content: &ToolChatUserContent) -> UserContent<'_> {
    if let Some(text) = content.as_text() {
        UserContent::Text(text)
    } else {
        UserContent::Parts(
            content
                .parts()
                .iter()
                .map(|part| match part {
                    ToolChatContentPart::Text { text } => UserContentPart::Text { text },
                    ToolChatContentPart::Image { data_url } => UserContentPart::ImageUrl {
                        image_url: ImageUrl {
                            url: data_url,
                            detail: "high",
                        },
                    },
                })
                .collect(),
        )
    }
}
