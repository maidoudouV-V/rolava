use super::config_store::AdminProviderConfig;
use super::error::ApiError;
use super::state::AdminState;
use crate::ai_provider::{
    gemini::GeminiProvider, openai_compatible::OpenAICompatibleProvider,
    openai_responses::OpenAIResponsesProvider, openrouter::OpenRouterProvider, AIProvider,
    ExtraBody, ToolChatMessage, ToolChatUserContent,
};
use crate::config::ModelConfig;
use crate::model_metadata::ModelPricing;
use crate::request_statistics::{with_request_purpose, RequestPurpose, RequestRecorder};
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

pub(super) fn routes() -> Router<Arc<AdminState>> {
    Router::new()
        .route("/test/model", post(test_model))
        .route("/providers/models", post(provider_models))
}

#[derive(Deserialize)]
struct TestModelRequest {
    provider: AdminProviderConfig,
    model: ModelConfig,
}

async fn test_model(
    State(state): State<Arc<AdminState>>,
    Json(request): Json<TestModelRequest>,
) -> Result<Json<Value>, ApiError> {
    let key = resolve_provider_key(&state, &request.provider);
    let extra_body = ExtraBody::parse(&request.model.extra_body)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let recorder = RequestRecorder::new(
        state.request_statistics.clone(),
        request.provider.name.clone(),
        request.model.name.clone(),
    )
    .with_pricing(
        state.request_pricing.clone(),
        request.provider.base_url.clone(),
    );
    let mut provider: Box<dyn AIProvider + Send + Sync> = match request.provider.r#type.as_str() {
        "openai_compatible" => Box::new(
            OpenAICompatibleProvider::new(
                key,
                request.provider.base_url,
                request.model.model,
                request.model.max_tokens,
                request.model.reasoning_effort,
            )
            .with_extra_body(extra_body),
        ),
        "openai_responses" => Box::new(
            OpenAIResponsesProvider::new(
                key,
                request.provider.base_url,
                request.model.model,
                request.model.max_tokens,
                request.model.reasoning_effort,
            )
            .with_extra_body(extra_body),
        ),
        "openrouter" => Box::new(
            OpenRouterProvider::new(
                key,
                request.provider.base_url,
                request.model.model,
                request.model.max_tokens,
                request.model.reasoning_effort,
            )
            .with_extra_body(extra_body),
        ),
        "gemini" => Box::new(
            GeminiProvider::new(
                key,
                request.provider.base_url,
                request.model.model,
                request.model.max_tokens,
                request.model.reasoning_effort,
            )
            .with_extra_body(extra_body),
        ),
        _ => return Err(ApiError::bad_request("不支持的 Provider 类型")),
    };
    provider.set_request_recorder(recorder);
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        with_request_purpose(
            RequestPurpose::ModelTest,
            provider.chat_completions(
                &[ToolChatMessage::User {
                    content: ToolChatUserContent::text("仅回复 OK"),
                }],
                &[],
            ),
        ),
    )
    .await
    .map_err(|_| ApiError::new(StatusCode::GATEWAY_TIMEOUT, "模型测试请求超时"))??;
    Ok(Json(
        json!({ "ok": true, "content": response.content, "model": response.model }),
    ))
}

#[derive(Deserialize)]
struct ProviderModelsRequest {
    provider: AdminProviderConfig,
}

#[derive(Serialize)]
struct ProviderModelItem {
    id: String,
    name: String,
    /// None 表示没有匹配到可靠的图像输入能力信息。
    vision: Option<bool>,
    pricing: ModelPricing,
}

async fn provider_models(
    State(state): State<Arc<AdminState>>,
    Json(request): Json<ProviderModelsRequest>,
) -> Result<Json<Value>, ApiError> {
    let key = resolve_provider_key(&state, &request.provider);
    let mut items = match request.provider.r#type.as_str() {
        "openai_compatible" | "openai_responses" | "openrouter" => {
            fetch_openai_style_models(&request.provider, &key).await?
        }
        "gemini" => fetch_gemini_models(&request.provider, &key).await?,
        _ => return Err(ApiError::bad_request("不支持的 Provider 类型")),
    };
    if request.provider.r#type == "openrouter" {
        state.request_pricing.update_openrouter(
            &request.provider.base_url,
            items
                .iter()
                .map(|item| (item.id.clone(), item.pricing.clone())),
        );
    } else {
        for item in &mut items {
            if let Some(metadata) = state.model_metadata.lookup(
                &request.provider.r#type,
                &request.provider.base_url,
                &item.id,
            ) {
                item.vision = item.vision.or(metadata.vision);
                item.pricing.merge_missing(&metadata.pricing);
            }
        }
    }
    Ok(Json(json!({ "items": items })))
}

fn resolve_provider_key(state: &AdminState, provider: &AdminProviderConfig) -> String {
    let app_config = state.app_config();
    provider
        .key
        .as_ref()
        .filter(|key| !key.is_empty())
        .cloned()
        .or_else(|| {
            let original_name = provider.original_name.as_deref().unwrap_or(&provider.name);
            app_config
                .providers
                .iter()
                .find(|saved| saved.name == original_name)
                .map(|saved| saved.key.clone())
        })
        .unwrap_or_default()
}

async fn fetch_openai_style_models(
    provider: &AdminProviderConfig,
    key: &str,
) -> Result<Vec<ProviderModelItem>, ApiError> {
    let default_base = if provider.r#type == "openrouter" {
        "https://openrouter.ai/api/v1"
    } else {
        "https://api.openai.com/v1"
    };
    let base_url = if provider.base_url.trim().is_empty() {
        default_base
    } else {
        provider.base_url.trim().trim_end_matches('/')
    };
    let client = reqwest::Client::new();
    let mut request = client
        .get(format!("{}/models", base_url))
        .timeout(Duration::from_secs(30));
    if !key.is_empty() {
        request = request.bearer_auth(key);
    }
    let response = request.send().await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("获取模型列表失败：HTTP {}，响应 {}", status, body),
        ));
    }
    let value: Value = serde_json::from_str(&body).map_err(|error| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("解析模型列表失败：{}", error),
        )
    })?;
    let models = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_GATEWAY, "模型列表响应缺少 data 数组"))?;
    Ok(normalize_model_items(
        models,
        provider.r#type == "openrouter",
    ))
}

async fn fetch_gemini_models(
    provider: &AdminProviderConfig,
    key: &str,
) -> Result<Vec<ProviderModelItem>, ApiError> {
    let base_url = if provider.base_url.trim().is_empty() {
        "https://generativelanguage.googleapis.com/v1beta"
    } else {
        provider.base_url.trim().trim_end_matches('/')
    };
    let client = reqwest::Client::new();
    let mut page_token: Option<String> = None;
    let mut items = Vec::new();
    // Google 单页最多返回 1000 个模型；按 nextPageToken 继续读取，避免截断目录。
    for _ in 0..20 {
        let mut request = client
            .get(format!("{}/models", base_url))
            .timeout(Duration::from_secs(30))
            .query(&[("pageSize", "1000")]);
        if let Some(token) = page_token.as_deref() {
            request = request.query(&[("pageToken", token)]);
        }
        if !key.is_empty() {
            request = request.header("x-goog-api-key", key);
        }
        let response = request.send().await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("获取 Google 模型列表失败：HTTP {}，响应 {}", status, body),
            ));
        }
        let value: Value = serde_json::from_str(&body).map_err(|error| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("解析 Google 模型列表失败：{}", error),
            )
        })?;
        let models = value
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "Google 模型列表响应缺少 models 数组",
                )
            })?;
        items.extend(
            models
                .iter()
                .filter(|model| is_gemini_text_generation_model(model))
                .flat_map(|model| normalize_model_items(std::slice::from_ref(model), false)),
        );
        page_token = value
            .get("nextPageToken")
            .and_then(Value::as_str)
            .map(str::to_string);
        if page_token.is_none() {
            break;
        }
    }
    Ok(items)
}

fn is_gemini_text_generation_model(model: &Value) -> bool {
    let supports_generation = model
        .get("supportedGenerationMethods")
        .and_then(Value::as_array)
        .is_some_and(|methods| {
            methods
                .iter()
                .any(|method| method.as_str() == Some("generateContent"))
        });
    if !supports_generation {
        return false;
    }
    let Some(id) = model
        .get("id")
        .or_else(|| model.get("name"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    // 图片生成、语音等专用模型也可能支持 generateContent，按型号词段排除。
    !id.to_ascii_lowercase()
        .split(['-', '_', '.', '/'])
        .any(|part| {
            matches!(
                part,
                "image"
                    | "imagen"
                    | "veo"
                    | "lyria"
                    | "banana"
                    | "omni"
                    | "tts"
                    | "audio"
                    | "live"
                    | "transcribe"
                    | "embedding"
                    | "embeddings"
                    | "aqa"
            )
        })
}

fn normalize_model_items(models: &[Value], openrouter: bool) -> Vec<ProviderModelItem> {
    models
        .iter()
        .filter_map(|model| {
            let raw_id = model
                .get("id")
                .or_else(|| model.get("name"))
                .and_then(Value::as_str)?;
            let id = raw_id.trim_start_matches("models/").to_string();
            let name = model
                .get("displayName")
                .or_else(|| model.get("display_name"))
                .or_else(|| model.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .trim_start_matches("models/")
                .to_string();
            Some(ProviderModelItem {
                id,
                name,
                vision: infer_model_vision(model),
                pricing: if openrouter {
                    ModelPricing::from_openrouter(model)
                } else {
                    ModelPricing::default()
                },
            })
        })
        .collect()
}

fn infer_model_vision(model: &Value) -> Option<bool> {
    for path in ["/capabilities/vision", "/vision"] {
        if let Some(value) = model.pointer(path).and_then(Value::as_bool) {
            return Some(value);
        }
    }
    for path in [
        "/architecture/input_modalities",
        "/modalities/input",
        "/input_modalities",
        "/modalities",
        "/capabilities/input_modalities",
    ] {
        if let Some(modalities) = model.pointer(path).and_then(Value::as_array) {
            return Some(modalities.iter().any(|modality| {
                modality
                    .as_str()
                    .is_some_and(|value| value.eq_ignore_ascii_case("image"))
            }));
        }
    }
    None
}
