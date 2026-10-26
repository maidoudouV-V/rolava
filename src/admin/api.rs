use super::auth::{require_admin, verify_auth};
use super::config_store::{
    list_prompt_files, read_prompt, write_admin_config, write_prompt, AdminConfigUpdate,
    AdminConfigView,
};
use super::error::ApiError;
use super::state::AdminState;
use crate::tools::ToolRegistry;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{Local, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;

pub fn router(state: Arc<AdminState>) -> Router {
    let api = Router::new()
        .route("/auth/verify", post(verify_auth))
        .route("/status", get(status))
        .route(
            "/request-statistics",
            get(super::request_statistics::get_statistics),
        )
        .route("/logs", get(logs))
        .route("/config", get(get_config).put(put_config))
        .route("/test/onebot", post(test_onebot))
        .merge(super::models::routes())
        .route("/prompts", get(prompts))
        .route("/prompts/{prompt_id}", get(get_prompt).put(put_prompt))
        .merge(super::skills::routes())
        .route("/resources/images/{image_id}", get(image_resource))
        .merge(super::conversations::routes())
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
    group_memories: u64,
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
        group_memories: stats.group_memories,
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

pub(super) fn schedule_restart(token: &CancellationToken) {
    let token = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(350)).await;
        token.cancel();
    });
}
