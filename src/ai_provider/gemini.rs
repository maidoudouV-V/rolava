use crate::ai_provider::{
    context_messages_without_tools, AIProvider, ChatUsage, ContextMessage, MessageRole,
    ReasoningState, ToolChatMessage, ToolChatResponse,
};
use crate::tools::ToolDefinition;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::trace;

mod chat;

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

pub struct GeminiProvider {
    http_client: Client,
    api_key: String,
    base_url: String,
    model: String,
    max_tokens: Option<i32>,
    reasoning_effort: String,
}

impl GeminiProvider {
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
            base_url.trim_end_matches('/').to_string()
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

    fn generate_content_url(&self) -> String {
        let model = self.model.trim_start_matches("models/");
        format!("{}/models/{}:generateContent", self.base_url, model)
    }

    fn request_body(&self, body: &impl Serialize) -> anyhow::Result<Value> {
        let mut body = serde_json::to_value(body)?;
        if let Some(thinking) = thinking_config(&self.reasoning_effort)? {
            if body.get("generationConfig").is_none() {
                body["generationConfig"] = serde_json::json!({});
            }
            body["generationConfig"]["thinkingConfig"] = thinking;
        }
        Ok(body)
    }
}

fn thinking_config(effort: &str) -> anyhow::Result<Option<Value>> {
    use serde_json::json;
    let level = match effort.trim() {
        "auto" => return Ok(None),
        "none" | "minimal" => "minimal",
        "low" => "low",
        "medium" => "medium",
        "high" | "xhigh" => "high",
        other => return Err(anyhow!("未知的思考强度：{}", other)),
    };
    Ok(Some(json!({"thinkingLevel": level})))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerateContentRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent>,
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiTool>>,
    #[serde(rename = "generationConfig")]
    generation_config: GeminiGenerationConfig,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiTool {
    #[serde(skip_serializing_if = "Option::is_none")]
    google_search: Option<GeminiGoogleSearch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url_context: Option<GeminiUrlContext>,
}

#[derive(Serialize)]
struct GeminiGoogleSearch {}
#[derive(Serialize)]
struct GeminiUrlContext {}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    #[serde(rename = "maxOutputTokens", skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_mime_type: Option<&'static str>,
    #[serde(rename = "mediaResolution", skip_serializing_if = "Option::is_none")]
    media_resolution: Option<&'static str>,
}

#[derive(Serialize, Deserialize, Clone)]
struct GeminiContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'static str>,
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inline_data: Option<GeminiInlineData>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct GeminiInlineData {
    mime_type: String,
    data: String,
}

#[derive(Deserialize)]
struct GeminiGenerateContentResponse {
    candidates: Option<Vec<GeminiCandidate>>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiResponseContent>,
}

#[derive(Deserialize)]
struct GeminiResponseContent {
    parts: Option<Vec<GeminiResponsePart>>,
}

#[derive(Deserialize)]
struct GeminiResponsePart {
    text: Option<String>,
    #[serde(default)]
    thought: bool,
}

#[async_trait]
impl AIProvider for GeminiProvider {
    async fn chat_completions(
        &self,
        messages: &[ToolChatMessage],
        tools: &[ToolDefinition],
    ) -> anyhow::Result<ToolChatResponse> {
        let body = chat::build_request(messages, tools, self.max_tokens)?;
        trace!(
            provider = "gemini",
            model = %self.model,
            request = %serde_json::to_string_pretty(&body)
                .unwrap_or_else(|error| format!("序列化请求失败: {}", error)),
            "AI Provider 完整请求"
        );
        let response_text = self.send_generate_content(&body).await?;
        chat::parse_response(&response_text, &self.model)
    }

    async fn describe_image(&self, image_data_url: &str, prompt: &str) -> anyhow::Result<String> {
        let image = parse_data_url(image_data_url)?;
        let image_data_bytes = image.data.len();
        let image_mime_type = image.mime_type.clone();
        let body = GeminiGenerateContentRequest {
            system_instruction: None,
            contents: vec![GeminiContent {
                role: Some("user"),
                parts: vec![
                    GeminiPart {
                        text: Some(prompt.to_string()),
                        inline_data: None,
                    },
                    GeminiPart {
                        text: None,
                        inline_data: Some(image),
                    },
                ],
            }],
            tools: None,
            generation_config: GeminiGenerationConfig {
                max_output_tokens: self.max_tokens,
                response_mime_type: None,
                media_resolution: Some("MEDIA_RESOLUTION_HIGH"),
            },
        };
        trace!(
            provider = "gemini",
            model = %self.model,
            prompt,
            image_data_bytes,
            mime_type = %image_mime_type,
            "视觉 Provider 请求"
        );

        let response_text = self.send_generate_content(&body).await?;
        let parsed: GeminiGenerateContentResponse = serde_json::from_str(&response_text)
            .map_err(|err| anyhow!("解析 Gemini 视觉响应失败：{}", err))?;
        extract_gemini_text(&parsed).ok_or_else(|| anyhow!("Gemini 视觉响应内容为空"))
    }

    async fn web_search(&self, question: &str) -> anyhow::Result<String> {
        let body = self.web_search_request(question);
        trace!(
            provider = "gemini",
            model = %self.model,
            request = %serde_json::to_string_pretty(&body)
                .unwrap_or_else(|error| format!("序列化请求失败: {}", error)),
            "联网搜索 Provider 完整请求"
        );

        let response_text = self.send_generate_content(&body).await?;
        let parsed: GeminiGenerateContentResponse = serde_json::from_str(&response_text)
            .map_err(|err| anyhow!("解析 Gemini 联网搜索响应失败：{}", err))?;
        let answer =
            extract_gemini_text(&parsed).ok_or_else(|| anyhow!("Gemini 联网搜索响应内容为空"))?;
        Ok(answer)
    }
}

impl GeminiProvider {
    fn web_search_request(&self, question: &str) -> GeminiGenerateContentRequest {
        GeminiGenerateContentRequest {
            system_instruction: None,
            contents: vec![GeminiContent {
                role: Some("user"),
                parts: vec![GeminiPart {
                    text: Some(question.to_string()),
                    inline_data: None,
                }],
            }],
            tools: Some(vec![
                GeminiTool {
                    google_search: Some(GeminiGoogleSearch {}),
                    url_context: None,
                },
                GeminiTool {
                    google_search: None,
                    url_context: Some(GeminiUrlContext {}),
                },
            ]),
            generation_config: GeminiGenerationConfig {
                max_output_tokens: self.max_tokens,
                response_mime_type: None,
                media_resolution: None,
            },
        }
    }

    async fn send_generate_content(
        &self,
        body: &(impl Serialize + Sync),
    ) -> anyhow::Result<String> {
        let body = self.request_body(body)?;
        let url = self.generate_content_url();
        trace!(provider = "gemini", model = %self.model, thinking_config = ?body.pointer("/generationConfig/thinkingConfig"), "Gemini 思考配置");
        let resp = self
            .http_client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Gemini 请求失败：{}", url))?;

        let status = resp.status();
        let response_text = resp
            .text()
            .await
            .with_context(|| format!("读取 Gemini 响应失败：{}，HTTP {}", url, status))?;
        trace!(provider = "gemini", status = %status, response = %response_text, "AI Provider 原始响应");
        if !status.is_success() {
            return Err(anyhow!(
                "Gemini API 调用失败，URL {}，状态码 {}：{}",
                url,
                status,
                response_text
            ));
        }

        let raw: Value =
            serde_json::from_str(&response_text).context("解析 Gemini 响应 JSON 失败")?;
        chat::validate_response(&raw)?;

        Ok(response_text)
    }
}

fn build_gemini_chat_contents(
    request_messages: &[ContextMessage],
) -> (Option<GeminiContent>, Vec<GeminiContent>) {
    let mut system_texts = Vec::new();
    let mut contents = Vec::new();

    for message in request_messages {
        match message.role {
            MessageRole::System => system_texts.push(message.content.as_str()),
            MessageRole::User => push_gemini_text_content(&mut contents, "user", &message.content),
            MessageRole::Assistant => {
                push_gemini_text_content(&mut contents, "model", &message.content)
            }
        }
    }

    let system_instruction = if system_texts.is_empty() {
        None
    } else {
        Some(GeminiContent {
            role: None,
            parts: vec![GeminiPart {
                text: Some(system_texts.join("\n\n")),
                inline_data: None,
            }],
        })
    };

    if contents.is_empty() {
        contents.push(GeminiContent {
            role: Some("user"),
            parts: vec![GeminiPart {
                text: Some(String::new()),
                inline_data: None,
            }],
        });
    }

    (system_instruction, contents)
}

fn push_gemini_text_content(contents: &mut Vec<GeminiContent>, role: &'static str, text: &str) {
    if let Some(last) = contents.last_mut() {
        if last.role == Some(role) {
            last.parts.push(GeminiPart {
                text: Some(text.to_string()),
                inline_data: None,
            });
            return;
        }
    }

    contents.push(GeminiContent {
        role: Some(role),
        parts: vec![GeminiPart {
            text: Some(text.to_string()),
            inline_data: None,
        }],
    });
}

fn extract_gemini_text(response: &GeminiGenerateContentResponse) -> Option<String> {
    let text = response
        .candidates
        .as_ref()?
        .first()?
        .content
        .as_ref()?
        .parts
        .as_ref()?
        .iter()
        .filter(|part| !part.thought)
        .filter_map(|part| part.text.as_deref())
        .collect::<Vec<_>>()
        .join("");
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn parse_data_url(image_data_url: &str) -> anyhow::Result<GeminiInlineData> {
    let Some(rest) = image_data_url.strip_prefix("data:") else {
        return Err(anyhow!("Gemini 视觉请求只支持 data URL 图片"));
    };
    let Some((mime_type, data)) = rest.split_once(";base64,") else {
        return Err(anyhow!("图片 data URL 缺少 ;base64, 分隔符"));
    };
    if mime_type.trim().is_empty() || data.trim().is_empty() {
        return Err(anyhow!("图片 data URL 的 MIME 类型或数据为空"));
    }

    Ok(GeminiInlineData {
        mime_type: mime_type.to_string(),
        data: data.to_string(),
    })
}
