use crate::ai_provider::{AIProvider, ChatResponse, ChatUsage, ContextMessage, MessageRole};
use anyhow::anyhow;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

pub struct GoogleAIStudioProvider {
    http_client: Client,
    api_key: String,
    base_url: String,
    model: String,
    max_tokens: i32,
}

impl GoogleAIStudioProvider {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        max_tokens: i32,
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
        }
    }

    fn generate_content_url(&self) -> String {
        let model = self.model.trim_start_matches("models/");
        format!("{}/models/{}:generateContent", self.base_url, model)
    }
}

#[derive(Serialize)]
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
struct GeminiGenerationConfig {
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_mime_type: Option<&'static str>,
}

#[derive(Serialize, Deserialize, Clone)]
struct GeminiContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'static str>,
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Deserialize, Clone)]
struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inline_data: Option<GeminiInlineData>,
}

#[derive(Serialize, Deserialize, Clone)]
struct GeminiInlineData {
    mime_type: String,
    data: String,
}

#[derive(Deserialize)]
struct GeminiGenerateContentResponse {
    candidates: Option<Vec<GeminiCandidate>>,
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiResponseContent>,
    #[serde(rename = "finishReason")]
    finish_reason: Option<String>,
    #[serde(rename = "groundingMetadata")]
    grounding_metadata: Option<GeminiGroundingMetadata>,
}

#[derive(Deserialize)]
struct GeminiResponseContent {
    parts: Option<Vec<GeminiResponsePart>>,
}

#[derive(Deserialize)]
struct GeminiResponsePart {
    text: Option<String>,
}

#[derive(Deserialize)]
struct GeminiGroundingMetadata {
    #[serde(rename = "webSearchQueries")]
    web_search_queries: Option<Vec<String>>,
    #[serde(rename = "groundingChunks")]
    grounding_chunks: Option<Vec<GeminiGroundingChunk>>,
}

#[derive(Deserialize)]
struct GeminiGroundingChunk {
    web: Option<GeminiGroundingWeb>,
}

#[derive(Deserialize)]
struct GeminiGroundingWeb {
    uri: Option<String>,
    title: Option<String>,
}

#[derive(Deserialize)]
struct GeminiUsageMetadata {
    #[serde(rename = "promptTokenCount")]
    prompt_token_count: Option<u64>,
    #[serde(rename = "candidatesTokenCount")]
    candidates_token_count: Option<u64>,
    #[serde(rename = "totalTokenCount")]
    total_token_count: Option<u64>,
}

#[async_trait]
impl AIProvider for GoogleAIStudioProvider {
    async fn chat_completions(
        &self,
        request_messages: &Vec<ContextMessage>,
    ) -> anyhow::Result<ChatResponse> {
        let (system_instruction, contents) = build_gemini_chat_contents(request_messages);
        let body = GeminiGenerateContentRequest {
            system_instruction,
            contents,
            tools: None,
            generation_config: GeminiGenerationConfig {
                max_output_tokens: self.max_tokens,
                response_mime_type: Some("application/json"),
            },
        };

        let response_text = self.send_generate_content(&body).await?;
        let raw_response: Value = serde_json::from_str(&response_text)?;
        let parsed: GeminiGenerateContentResponse =
            serde_json::from_str(&response_text).map_err(|err| {
                anyhow!(
                    "解析 Google AI Studio 响应失败：{}\n原始响应：\n{}",
                    err,
                    response_text
                )
            })?;
        let content =
            extract_gemini_text(&parsed).ok_or_else(|| anyhow!("Google AI Studio 响应内容为空"))?;
        let finish_reason = parsed
            .candidates
            .as_ref()
            .and_then(|candidates| candidates.first())
            .and_then(|candidate| candidate.finish_reason.clone());
        let usage = parsed.usage_metadata.map(|usage| ChatUsage {
            prompt_tokens: usage.prompt_token_count,
            completion_tokens: usage.candidates_token_count,
            total_tokens: usage.total_token_count,
        });

        Ok(ChatResponse {
            content,
            reasoning_content: None,
            finish_reason,
            id: None,
            model: Some(self.model.clone()),
            usage,
            raw_response,
        })
    }

    async fn describe_image(&self, image_data_url: &str, prompt: &str) -> anyhow::Result<String> {
        let image = parse_data_url(image_data_url)?;
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
            },
        };

        let response_text = self.send_generate_content(&body).await?;
        let parsed: GeminiGenerateContentResponse =
            serde_json::from_str(&response_text).map_err(|err| {
                anyhow!(
                    "解析 Google AI Studio 视觉响应失败：{}\n原始响应：\n{}",
                    err,
                    response_text
                )
            })?;
        extract_gemini_text(&parsed).ok_or_else(|| anyhow!("Google AI Studio 视觉响应内容为空"))
    }

    async fn web_search(&self, query: &str) -> anyhow::Result<String> {
        let prompt = format!(
            "你是一个给角色扮演聊天机器人使用的互联网查询工具。请使用 Google Search 查询实时信息，并给出简短、可靠、适合交给角色继续聊天使用的中文答案。\n\
要求：\n\
1. 直接回答查询问题，不要写成客服或长篇报告。\n\
2. 如果信息不确定，明确说明不确定。\n\
3. 控制在 600 字以内。\n\
4. 如果有关键来源，答案中可以简短提及来源名称。\n\n\
查询：{}",
            query
        );
        let body = GeminiGenerateContentRequest {
            system_instruction: None,
            contents: vec![GeminiContent {
                role: Some("user"),
                parts: vec![GeminiPart {
                    text: Some(prompt),
                    inline_data: None,
                }],
            }],
            tools: Some(vec![GeminiTool {
                google_search: Some(GeminiGoogleSearch {}),
                url_context: Some(GeminiUrlContext {}),
            }]),
            generation_config: GeminiGenerationConfig {
                max_output_tokens: self.max_tokens,
                response_mime_type: None,
            },
        };

        let response_text = self.send_generate_content(&body).await?;
        let parsed: GeminiGenerateContentResponse =
            serde_json::from_str(&response_text).map_err(|err| {
                anyhow!(
                    "解析 Google AI Studio 联网搜索响应失败：{}\n原始响应：\n{}",
                    err,
                    response_text
                )
            })?;
        let answer = extract_gemini_text(&parsed)
            .ok_or_else(|| anyhow!("Google AI Studio 联网搜索响应内容为空"))?;
        Ok(format_grounded_search_result(answer, &parsed))
    }
}

impl GoogleAIStudioProvider {
    async fn send_generate_content(
        &self,
        body: &GeminiGenerateContentRequest,
    ) -> anyhow::Result<String> {
        let resp = self
            .http_client
            .post(self.generate_content_url())
            .header("x-goog-api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let error_text = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Google AI Studio API 调用失败，状态码 {}：{}",
                status,
                error_text
            ));
        }

        Ok(resp.text().await?)
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
        .filter_map(|part| part.text.as_deref())
        .collect::<Vec<_>>()
        .join("");
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn format_grounded_search_result(
    answer: String,
    response: &GeminiGenerateContentResponse,
) -> String {
    let answer = limit_chars(answer.trim(), 900);
    let Some(metadata) = response
        .candidates
        .as_ref()
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate.grounding_metadata.as_ref())
    else {
        return answer;
    };

    let mut sections = vec![answer];
    if let Some(queries) = metadata
        .web_search_queries
        .as_ref()
        .filter(|queries| !queries.is_empty())
    {
        sections.push(format!("搜索词：{}", queries.join("；")));
    }
    let sources = metadata
        .grounding_chunks
        .as_ref()
        .map(|chunks| {
            chunks
                .iter()
                .filter_map(|chunk| chunk.web.as_ref())
                .filter_map(|web| {
                    let uri = web.uri.as_deref()?.trim();
                    if uri.is_empty() {
                        return None;
                    }
                    let title = web
                        .title
                        .as_deref()
                        .map(str::trim)
                        .filter(|title| !title.is_empty())
                        .unwrap_or("来源");
                    Some(format!("{}：{}", title, uri))
                })
                .take(3)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !sources.is_empty() {
        sections.push(format!("来源：\n{}", sources.join("\n")));
    }

    limit_chars(&sections.join("\n"), 1400)
}

fn limit_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let limited: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{}...", limited)
    } else {
        limited
    }
}

fn parse_data_url(image_data_url: &str) -> anyhow::Result<GeminiInlineData> {
    let Some(rest) = image_data_url.strip_prefix("data:") else {
        return Err(anyhow!("Google AI Studio 视觉请求只支持 data URL 图片"));
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
