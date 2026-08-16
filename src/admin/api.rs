use super::config_store::{
    list_prompt_files, read_prompt, write_admin_config, write_prompt, AdminConfigUpdate,
    AdminConfigView, AdminProviderConfig,
};
use super::log_buffer::AdminLogBuffer;
use crate::ai_provider::{
    google_aistudio::GoogleAIStudioProvider, openai_compatible::OpenAICompatibleProvider,
    openrouter::OpenRouterProvider, AIProvider, ToolChatMessage, ToolChatUserContent,
};
use crate::config::{AppConfig, ModelConfig};
use crate::memory::{
    CharacterMemorySession, UserMemoryService, MAX_RETENTION_DAYS, SECONDS_PER_DAY,
};
use crate::repository::db_manager::{ConversationRecord, QQChatContextManager};
use crate::runtime_state::{RuntimeGroupMember, RuntimeState};
use crate::scheduler::SchedulerService;
use crate::tools::ToolRegistry;
use crate::transport::message::{Conversation, ConversationKind, MessageTarget};
use crate::transport::onebot::OneBotHttpServer;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{Local, Timelike, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;

const GROUP_MEMBER_CACHE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone)]
pub struct AdminState {
    app_config: Arc<RwLock<Arc<AppConfig>>>,
    pub config_path: PathBuf,
    pub db_manager: Arc<QQChatContextManager>,
    pub scheduler: Arc<SchedulerService>,
    pub onebot: Arc<OneBotHttpServer>,
    pub runtime: Arc<RuntimeState>,
    pub logs: Arc<AdminLogBuffer>,
    pub restart: CancellationToken,
    pub started_at: i64,
    user_memory: Arc<UserMemoryService>,
}

impl AdminState {
    pub fn new(
        app_config: Arc<AppConfig>,
        config_path: PathBuf,
        db_manager: Arc<QQChatContextManager>,
        scheduler: Arc<SchedulerService>,
        onebot: Arc<OneBotHttpServer>,
        runtime: Arc<RuntimeState>,
        logs: Arc<AdminLogBuffer>,
        restart: CancellationToken,
    ) -> Self {
        Self {
            user_memory: Arc::new(UserMemoryService::new(db_manager.clone())),
            app_config: Arc::new(RwLock::new(app_config)),
            config_path,
            db_manager,
            scheduler,
            onebot,
            runtime,
            logs,
            restart,
            started_at: Utc::now().timestamp(),
        }
    }

    /// 管理后台使用可替换快照，普通保存后刷新页面也能读取最新配置。
    fn app_config(&self) -> Arc<AppConfig> {
        self.app_config.read().clone()
    }

    fn replace_app_config(&self, config: AppConfig) {
        *self.app_config.write() = Arc::new(config);
    }
}

pub fn router(state: Arc<AdminState>) -> Router {
    let api = Router::new()
        .route("/auth/verify", post(verify_auth))
        .route("/status", get(status))
        .route("/logs", get(logs))
        .route("/config", get(get_config).put(put_config))
        .route("/test/onebot", post(test_onebot))
        .route("/test/model", post(test_model))
        .route("/providers/models", post(provider_models))
        .route("/prompts", get(prompts))
        .route("/prompts/{prompt_id}", get(get_prompt).put(put_prompt))
        .route("/resources/images/{image_id}", get(image_resource))
        .route("/conversations", get(conversations))
        .route("/conversations/{conversation_id}", get(conversation_detail))
        .route(
            "/conversations/{conversation_id}/messages",
            get(conversation_messages),
        )
        .route(
            "/conversations/{conversation_id}/user-memories",
            get(user_memories).post(create_user_memory),
        )
        .route(
            "/conversations/{conversation_id}/users/{user_id}/memories/{memory_id}",
            put(update_user_memory).delete(delete_user_memory),
        )
        .route(
            "/conversations/{conversation_id}/character-memories",
            get(character_memories).post(create_character_memory),
        )
        .route(
            "/conversations/{conversation_id}/character-memories/{memory_id}",
            put(update_character_memory).delete(delete_character_memory),
        )
        .route(
            "/conversations/{conversation_id}/scheduled-tasks",
            get(scheduled_tasks).post(create_scheduled_task),
        )
        .route(
            "/conversations/{conversation_id}/scheduled-tasks/{task_id}",
            put(update_scheduled_task).delete(delete_scheduled_task),
        )
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin))
        .with_state(state);

    Router::new()
        .route("/admin", get(admin_path_redirect))
        .route("/admin/", get(admin_index))
        .nest_service("/admin/assets", ServeDir::new("web/assets"))
        .nest("/api/admin", api)
}

#[derive(Deserialize)]
struct LogQuery {
    after_id: Option<u64>,
    limit: Option<usize>,
}

async fn logs(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<LogQuery>,
) -> Json<super::log_buffer::AdminLogPage> {
    Json(
        state
            .logs
            .read_after(query.after_id, query.limit.unwrap_or(200)),
    )
}

async fn admin_path_redirect() -> Redirect {
    Redirect::temporary("/admin/")
}

async fn admin_index() -> Result<Html<String>, ApiError> {
    Ok(Html(tokio::fs::read_to_string("web/index.html").await?))
}

async fn require_admin(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let app_config = state.app_config();
    if bearer_token(&headers)
        .is_some_and(|token| constant_time_equals(token, &app_config.admin.token))
    {
        return next.run(request).await;
    }
    ApiError::new(StatusCode::UNAUTHORIZED, "管理 Token 无效").into_response()
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn constant_time_equals(actual: &str, expected: &str) -> bool {
    actual.len() == expected.len() && actual.as_bytes().ct_eq(expected.as_bytes()).into()
}

async fn verify_auth() -> Json<Value> {
    Json(json!({ "authenticated": true }))
}

#[derive(Serialize)]
struct StatusResponse {
    version: &'static str,
    started_at: i64,
    uptime_seconds: i64,
    bot_id: Option<String>,
    bot_name: Option<String>,
    onebot_online: Option<bool>,
    last_event_at: Option<i64>,
    conversations: u64,
    group_conversations: u64,
    direct_conversations: u64,
    messages_today: u64,
    user_memories: u64,
    character_memories: u64,
    scheduled_tasks: u64,
}

async fn status(State(state): State<Arc<AdminState>>) -> Result<Json<StatusResponse>, ApiError> {
    let now = Local::now();
    let today_start = now.timestamp() - i64::from(now.num_seconds_from_midnight());
    let stats = state.db_manager.admin_database_stats(today_start)?;
    Ok(Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION"),
        started_at: state.started_at,
        uptime_seconds: Utc::now().timestamp().saturating_sub(state.started_at),
        bot_id: state.runtime.bot_id(),
        bot_name: state.runtime.bot_name(),
        onebot_online: state.runtime.onebot_online(),
        last_event_at: state.runtime.last_event_at(),
        conversations: stats.conversations,
        group_conversations: stats.group_conversations,
        direct_conversations: stats.direct_conversations,
        messages_today: stats.messages_today,
        user_memories: stats.user_memories,
        character_memories: stats.character_memories,
        scheduled_tasks: stats.scheduled_tasks,
    }))
}

async fn get_config(State(state): State<Arc<AdminState>>) -> Json<Value> {
    let app_config = state.app_config();
    Json(json!({
        "config": AdminConfigView::from_config(&app_config),
        "optional_tools": ToolRegistry::optional_definitions(),
    }))
}

#[derive(Deserialize)]
struct SaveConfigQuery {
    #[serde(default)]
    restart: Option<bool>,
}

async fn put_config(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<SaveConfigQuery>,
    Json(update): Json<AdminConfigUpdate>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let path = state.config_path.clone();
    let app_config =
        tokio::task::spawn_blocking(move || write_admin_config(&path, update)).await??;
    state.replace_app_config(app_config);
    if query.restart.unwrap_or(true) {
        schedule_restart(&state.restart);
        return Ok((StatusCode::ACCEPTED, Json(json!({ "restarting": true }))));
    }
    Ok((StatusCode::OK, Json(json!({ "restarting": false }))))
}

#[derive(Deserialize)]
struct TestOneBotRequest {
    onebot_api: String,
    onebot_token: Option<String>,
}

async fn test_onebot(
    State(state): State<Arc<AdminState>>,
    Json(request): Json<TestOneBotRequest>,
) -> Result<Json<Value>, ApiError> {
    let app_config = state.app_config();
    let token = request
        .onebot_token
        .filter(|token| !token.is_empty())
        .unwrap_or_else(|| app_config.server.onebot_token.clone());
    let mut http_request = reqwest::Client::new()
        .post(format!(
            "{}/get_login_info",
            request.onebot_api.trim().trim_end_matches('/')
        ))
        .timeout(Duration::from_secs(15))
        .json(&json!({}));
    if !token.is_empty() {
        http_request = http_request.bearer_auth(token);
    }
    let response = http_request.send().await?;
    let status = response.status();
    let body: Value = response.json().await?;
    if !status.is_success() || body.get("retcode").and_then(Value::as_i64).unwrap_or(-1) != 0 {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("OneBot 测试失败：HTTP {}，响应 {}", status, body),
        ));
    }
    let bot_id = body
        .pointer("/data/user_id")
        .and_then(|value| {
            value
                .as_i64()
                .map(|value| value.to_string())
                .or_else(|| value.as_str().map(str::to_string))
        })
        .ok_or_else(|| ApiError::new(StatusCode::BAD_GATEWAY, "OneBot 响应缺少 data.user_id"))?;
    Ok(Json(json!({ "ok": true, "bot_id": bot_id.to_string() })))
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
    let provider: Box<dyn AIProvider + Send + Sync> = match request.provider.r#type.as_str() {
        "openai_compatible" => Box::new(OpenAICompatibleProvider::new(
            key,
            request.provider.base_url,
            request.model.model,
            request.model.max_tokens,
            request.model.reasoning_effort,
        )),
        "openrouter" => Box::new(OpenRouterProvider::new(
            key,
            request.provider.base_url,
            request.model.model,
            request.model.max_tokens,
            request.model.reasoning_effort,
        )),
        "google_aistudio" => Box::new(GoogleAIStudioProvider::new(
            key,
            request.provider.base_url,
            request.model.model,
            request.model.max_tokens,
        )),
        _ => return Err(ApiError::bad_request("不支持的 Provider 类型")),
    };
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        provider.chat_completions(
            &[ToolChatMessage::User {
                content: ToolChatUserContent::text("仅回复 OK"),
            }],
            &[],
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
    /// None 表示供应商目录没有提供可靠的图像输入能力信息。
    vision: Option<bool>,
}

async fn provider_models(
    State(state): State<Arc<AdminState>>,
    Json(request): Json<ProviderModelsRequest>,
) -> Result<Json<Value>, ApiError> {
    let key = resolve_provider_key(&state, &request.provider);
    let items = match request.provider.r#type.as_str() {
        "openai_compatible" | "openrouter" => {
            fetch_openai_style_models(&request.provider, &key).await?
        }
        "google_aistudio" => fetch_google_models(&request.provider, &key).await?,
        _ => return Err(ApiError::bad_request("不支持的 Provider 类型")),
    };
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
    Ok(normalize_model_items(models))
}

async fn fetch_google_models(
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
        items.extend(normalize_model_items(models));
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

fn normalize_model_items(models: &[Value]) -> Vec<ProviderModelItem> {
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

async fn prompts(State(state): State<Arc<AdminState>>) -> Result<Json<Value>, ApiError> {
    let app_config = state.app_config();
    Ok(Json(json!({ "items": list_prompt_files(&app_config)? })))
}

async fn get_prompt(
    State(state): State<Arc<AdminState>>,
    Path(prompt_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let app_config = state.app_config();
    Ok(Json(json!({
        "id": prompt_id,
        "content": read_prompt(&app_config, &prompt_id)?,
    })))
}

#[derive(Deserialize)]
struct PromptUpdate {
    content: String,
}

async fn put_prompt(
    State(state): State<Arc<AdminState>>,
    Path(prompt_id): Path<String>,
    Json(update): Json<PromptUpdate>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let app_config = state.app_config();
    write_prompt(&app_config, &prompt_id, &update.content)?;
    schedule_restart(&state.restart);
    Ok((StatusCode::ACCEPTED, Json(json!({ "restarting": true }))))
}

async fn image_resource(
    State(state): State<Arc<AdminState>>,
    Path(image_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    if !image_id
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return Err(ApiError::bad_request("图片 ID 不合法"));
    }
    let resource = state
        .db_manager
        .get_admin_image_resource(&image_id)?
        .ok_or_else(|| ApiError::not_found("图片不存在"))?;
    let data = tokio::fs::read(resource.local_path).await?;
    let content_type = resource
        .mime_type
        .unwrap_or_else(|| "application/octet-stream".to_string());
    Ok(([(axum::http::header::CONTENT_TYPE, content_type)], data))
}

fn schedule_restart(token: &CancellationToken) {
    let token = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(350)).await;
        token.cancel();
    });
}

#[derive(Deserialize)]
struct ConversationListQuery {
    kind: Option<String>,
    cursor: Option<String>,
    limit: Option<u32>,
}

#[derive(Serialize)]
struct ConversationSummaryResponse {
    id: i64,
    source_id: String,
    kind: String,
    title: String,
    member_count: Option<u64>,
    latest_sender_name: Option<String>,
    latest_content: Option<String>,
    latest_message_at: Option<i64>,
    unread_count: u64,
}

async fn conversations(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<ConversationListQuery>,
) -> Result<Json<Value>, ApiError> {
    let cursor = query.cursor.as_deref().map(parse_cursor).transpose()?;
    let rows = state.db_manager.list_admin_conversations(
        query.kind.as_deref().filter(|kind| *kind != "all"),
        cursor,
        query.limit.unwrap_or(30),
    )?;
    let requested_limit = query.limit.unwrap_or(30).clamp(1, 100) as usize;
    let next_cursor = (rows.len() == requested_limit)
        .then(|| {
            rows.last().map(|row| {
                format!(
                    "{}:{}",
                    row.conversation.last_message_at, row.conversation.id
                )
            })
        })
        .flatten();
    let items = rows
        .into_iter()
        .map(|row| {
            let group = (row.conversation.kind == "group")
                .then(|| {
                    state
                        .runtime
                        .group(&row.conversation.source_conversation_id)
                })
                .flatten();
            ConversationSummaryResponse {
                id: row.conversation.id,
                source_id: row.conversation.source_conversation_id.clone(),
                kind: row.conversation.kind,
                title: group
                    .as_ref()
                    .map(|group| group.name.clone())
                    .or(row.conversation.title)
                    .unwrap_or(row.conversation.source_conversation_id),
                member_count: group.map(|group| group.member_count),
                latest_sender_name: row.latest_sender_name,
                latest_content: row.latest_content,
                latest_message_at: row.latest_message_at,
                unread_count: row.unread_count,
            }
        })
        .collect::<Vec<_>>();
    let stats = state.db_manager.admin_database_stats(0)?;
    Ok(Json(json!({
        "items": items,
        "next_cursor": next_cursor,
        "counts": {
            "all": stats.conversations,
            "group": stats.group_conversations,
            "direct": stats.direct_conversations,
        }
    })))
}

fn parse_cursor(cursor: &str) -> Result<(i64, i64), ApiError> {
    let (timestamp, id) = cursor
        .split_once(':')
        .ok_or_else(|| ApiError::bad_request("会话游标无效"))?;
    Ok((timestamp.parse()?, id.parse()?))
}

async fn conversation_detail(
    State(state): State<Arc<AdminState>>,
    Path(conversation_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    let group = (conversation.kind == "group")
        .then(|| state.runtime.group(&conversation.source_conversation_id))
        .flatten();
    Ok(Json(json!({
        "id": conversation.id,
        "source": conversation.source,
        "source_id": conversation.source_conversation_id,
        "kind": conversation.kind,
        "title": group.as_ref().map(|value| value.name.clone()).or(conversation.title),
        "member_count": group.as_ref().map(|value| value.member_count),
        "max_member_count": group.and_then(|value| value.max_member_count),
        "last_message_at": conversation.last_message_at,
    })))
}

#[derive(Deserialize)]
struct MessageListQuery {
    before_id: Option<i64>,
    after_id: Option<i64>,
    limit: Option<u32>,
}

async fn conversation_messages(
    State(state): State<Arc<AdminState>>,
    Path(conversation_id): Path<i64>,
    Query(query): Query<MessageListQuery>,
) -> Result<Json<Value>, ApiError> {
    get_conversation(&state, conversation_id)?;
    if query.before_id.is_some() && query.after_id.is_some() {
        return Err(ApiError::bad_request("before_id 和 after_id 不能同时使用"));
    }
    let bot_id = state.runtime.bot_id();
    let bot_name = state.runtime.bot_name();
    let messages = state.db_manager.get_admin_conversation_messages(
        conversation_id,
        query.before_id,
        query.after_id,
        query.limit.unwrap_or(50),
    )?;
    let next_before_id = query
        .after_id
        .is_none()
        .then(|| messages.first().map(|message| message.id))
        .flatten();
    let items = messages
        .into_iter()
        .map(|message| {
            let is_bot = bot_id.as_deref() == Some(message.sender_id.as_str());
            let sender_name = if is_bot {
                bot_name
                    .as_deref()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or(&message.sender_display_name)
                    .to_string()
            } else {
                message.sender_display_name.clone()
            };
            json!({
                "id": message.id,
                "sender_id": message.sender_id,
                "sender_name": sender_name,
                "content": message.content_text.unwrap_or_default(),
                "parts": serde_json::from_str::<Value>(&message.content_parts_json).unwrap_or(Value::Array(Vec::new())),
                "is_bot": is_bot,
                "is_read": message.is_read,
                "timestamp": message.event_timestamp,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(
        json!({ "items": items, "next_before_id": next_before_id }),
    ))
}

#[derive(Serialize)]
struct MemberMemoryResponse {
    user_id: String,
    nickname: String,
    card: String,
    role: String,
    memories: Vec<Value>,
}

async fn user_memories(
    State(state): State<Arc<AdminState>>,
    Path(conversation_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    let (members, stale) = conversation_members(&state, &conversation).await?;
    let bot_id = require_bot_id(&state)?;
    let user_ids = members
        .iter()
        .map(|member| member.user_id.clone())
        .collect::<Vec<_>>();
    let memories =
        state
            .db_manager
            .get_user_memories_for_users(&conversation.source, &bot_id, &user_ids)?;
    let mut by_user = HashMap::<String, Vec<Value>>::new();
    for memory in memories {
        by_user.entry(memory.user_id).or_default().push(json!({
            "id": memory.memory_id,
            "content": memory.content,
        }));
    }
    let users = members
        .into_iter()
        .map(|member| MemberMemoryResponse {
            memories: by_user.remove(&member.user_id).unwrap_or_default(),
            user_id: member.user_id,
            nickname: member.nickname,
            card: member.card,
            role: member.role,
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "users": users, "stale": stale })))
}

#[derive(Deserialize)]
struct UserMemoryCreate {
    user_id: String,
    content: String,
}

async fn create_user_memory(
    State(state): State<Arc<AdminState>>,
    Path(conversation_id): Path<i64>,
    Json(request): Json<UserMemoryCreate>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    ensure_conversation_member(&state, &conversation, &request.user_id).await?;
    let memory_id = state.user_memory.create(
        &conversation.source,
        &require_bot_id(&state)?,
        &request.user_id,
        &request.content,
    )?;
    Ok((StatusCode::CREATED, Json(json!({ "id": memory_id }))))
}

#[derive(Deserialize)]
struct UserMemoryUpdate {
    content: String,
}

async fn update_user_memory(
    State(state): State<Arc<AdminState>>,
    Path((conversation_id, user_id, memory_id)): Path<(i64, String, String)>,
    Json(request): Json<UserMemoryUpdate>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    ensure_conversation_member(&state, &conversation, &user_id).await?;
    state.user_memory.update(
        &conversation.source,
        &require_bot_id(&state)?,
        &user_id,
        &memory_id,
        &request.content,
    )?;
    Ok(Json(json!({ "updated": true })))
}

async fn delete_user_memory(
    State(state): State<Arc<AdminState>>,
    Path((conversation_id, user_id, memory_id)): Path<(i64, String, String)>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    state.user_memory.delete(
        &conversation.source,
        &require_bot_id(&state)?,
        &user_id,
        &memory_id,
    )?;
    Ok(Json(json!({ "deleted": true })))
}

async fn character_memories(
    State(state): State<Arc<AdminState>>,
    Path(conversation_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    let records = state.db_manager.get_character_memories(
        &conversation.source,
        &require_bot_id(&state)?,
        &conversation.source_conversation_id,
    )?;
    let now = Utc::now().timestamp();
    let items = records.into_iter().map(|memory| json!({
        "id": memory.id,
        "title": memory.title,
        "content": memory.content,
        "expires_at": memory.expires_at,
        "remaining_days": ((memory.expires_at - now).max(0) + SECONDS_PER_DAY - 1) / SECONDS_PER_DAY,
        "expiring": memory.expires_at <= now,
    })).collect::<Vec<_>>();
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
struct CharacterMemoryWrite {
    title: String,
    content: String,
    retention_days: u16,
}

async fn create_character_memory(
    State(state): State<Arc<AdminState>>,
    Path(conversation_id): Path<i64>,
    Json(request): Json<CharacterMemoryWrite>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    let session = CharacterMemorySession::new(
        message_target(&state, &conversation)?,
        state.db_manager.clone(),
    );
    let result = session.set_memory(
        &request.title,
        Some(&request.content),
        Some(request.retention_days),
    )?;
    Ok((StatusCode::CREATED, Json(json!({ "message": result }))))
}

async fn update_character_memory(
    State(state): State<Arc<AdminState>>,
    Path((conversation_id, memory_id)): Path<(i64, i64)>,
    Json(request): Json<CharacterMemoryWrite>,
) -> Result<Json<Value>, ApiError> {
    CharacterMemorySession::validate_title(request.title.trim())?;
    CharacterMemorySession::validate_content(request.content.trim())?;
    if !(1..=MAX_RETENTION_DAYS).contains(&request.retention_days) {
        return Err(ApiError::bad_request("角色记忆时间必须在 1 到 365 天之间"));
    }
    let conversation = get_conversation(&state, conversation_id)?;
    let expires_at = Utc::now().timestamp() + i64::from(request.retention_days) * SECONDS_PER_DAY;
    let updated = state.db_manager.update_character_memory_by_id(
        &conversation.source,
        &require_bot_id(&state)?,
        &conversation.source_conversation_id,
        memory_id,
        request.title.trim(),
        request.content.trim(),
        expires_at,
    )?;
    if !updated {
        return Err(ApiError::not_found("角色记忆不存在"));
    }
    Ok(Json(json!({ "updated": true })))
}

async fn delete_character_memory(
    State(state): State<Arc<AdminState>>,
    Path((conversation_id, memory_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    let deleted = state.db_manager.delete_character_memory_by_id(
        &conversation.source,
        &require_bot_id(&state)?,
        &conversation.source_conversation_id,
        memory_id,
    )?;
    if !deleted {
        return Err(ApiError::not_found("角色记忆不存在"));
    }
    Ok(Json(json!({ "deleted": true })))
}

async fn scheduled_tasks(
    State(state): State<Arc<AdminState>>,
    Path(conversation_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    let target = message_target(&state, &conversation)?;
    let items = state
        .scheduler
        .running_tasks(&target)?
        .into_iter()
        .map(|task| {
            json!({
                "id": task.id,
                "title": task.title,
                "schedule": task.schedule,
                "instruction": task.instruction,
                "next_run_at": task.next_run_at,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
struct ScheduledTaskWrite {
    title: String,
    schedule: String,
    instruction: String,
}

async fn create_scheduled_task(
    State(state): State<Arc<AdminState>>,
    Path(conversation_id): Path<i64>,
    Json(request): Json<ScheduledTaskWrite>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    let task = state.scheduler.create_task(
        &message_target(&state, &conversation)?,
        &request.title,
        &request.schedule,
        &request.instruction,
    )?;
    Ok((StatusCode::CREATED, Json(json!({ "id": task.id }))))
}

async fn update_scheduled_task(
    State(state): State<Arc<AdminState>>,
    Path((conversation_id, task_id)): Path<(i64, String)>,
    Json(request): Json<ScheduledTaskWrite>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    state.scheduler.update_task(
        &message_target(&state, &conversation)?,
        &task_id,
        Some(&request.title),
        Some(&request.schedule),
        Some(&request.instruction),
    )?;
    Ok(Json(json!({ "updated": true })))
}

async fn delete_scheduled_task(
    State(state): State<Arc<AdminState>>,
    Path((conversation_id, task_id)): Path<(i64, String)>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    let deleted = state
        .scheduler
        .delete_task(&message_target(&state, &conversation)?, &task_id)?;
    if !deleted {
        return Err(ApiError::not_found("定时任务不存在"));
    }
    Ok(Json(json!({ "deleted": true })))
}

fn get_conversation(
    state: &AdminState,
    conversation_id: i64,
) -> Result<ConversationRecord, ApiError> {
    state
        .db_manager
        .get_conversation_by_id(conversation_id)?
        .ok_or_else(|| ApiError::not_found("会话不存在"))
}

fn require_bot_id(state: &AdminState) -> Result<String, ApiError> {
    state
        .runtime
        .bot_id()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "尚未获取 Bot QQ 号"))
}

fn message_target(
    state: &AdminState,
    conversation: &ConversationRecord,
) -> Result<MessageTarget, ApiError> {
    let kind = match conversation.kind.as_str() {
        "group" => ConversationKind::Group,
        "direct" => ConversationKind::Direct,
        _ => return Err(ApiError::bad_request("会话类型无效")),
    };
    Ok(MessageTarget {
        source: conversation.source.clone(),
        bot_id: require_bot_id(state)?,
        conversation: Conversation {
            id: conversation.source_conversation_id.clone(),
            kind,
            title: conversation.title.clone(),
        },
    })
}

async fn conversation_members(
    state: &AdminState,
    conversation: &ConversationRecord,
) -> Result<(Vec<RuntimeGroupMember>, bool), ApiError> {
    if conversation.kind == "direct" {
        return Ok((
            vec![RuntimeGroupMember {
                user_id: conversation.source_conversation_id.clone(),
                nickname: conversation
                    .title
                    .clone()
                    .unwrap_or_else(|| conversation.source_conversation_id.clone()),
                card: String::new(),
                role: "friend".to_string(),
            }],
            false,
        ));
    }
    let group_id = conversation.source_conversation_id.parse::<i64>()?;
    let cached = state
        .runtime
        .cached_group_members(&conversation.source_conversation_id);
    if cached
        .as_ref()
        .is_some_and(|entry| entry.is_fresh(GROUP_MEMBER_CACHE_TTL))
    {
        return Ok((cached.unwrap().members, false));
    }
    match state.onebot.fetch_group_members(group_id).await {
        Ok(mut members) => {
            if let Some(bot_id) = state.runtime.bot_id() {
                members.retain(|member| member.user_id != bot_id);
            }
            state
                .runtime
                .cache_group_members(&conversation.source_conversation_id, members.clone());
            Ok((members, false))
        }
        Err(error) => cached.map(|entry| (entry.members, true)).ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("获取群成员失败：{:#}", error),
            )
        }),
    }
}

async fn ensure_conversation_member(
    state: &AdminState,
    conversation: &ConversationRecord,
    user_id: &str,
) -> Result<(), ApiError> {
    let (members, _) = conversation_members(state, conversation).await?;
    if members
        .iter()
        .any(|member| member.user_id == user_id.trim())
    {
        Ok(())
    } else {
        Err(ApiError::bad_request("指定 QQ 号不是当前会话成员"))
    }
}

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl<E> From<E> for ApiError
where
    E: Into<anyhow::Error>,
{
    fn from(error: E) -> Self {
        let error = error.into();
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", error))
    }
}
