use super::api::schedule_restart;
use super::error::ApiError;
use super::skill_store::{
    list_skill_files, list_skills, normalize_enabled_names, read_skill_file, write_skill_file,
};
use super::state::AdminState;
use crate::config::{validate_skill_environment_groups, SkillEnvironmentGroups};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashSet, path::PathBuf, sync::Arc};

pub(super) fn routes() -> Router<Arc<AdminState>> {
    Router::new()
        .route("/skills", get(admin_skills))
        .route("/skills/settings", put(put_skill_settings))
        .route(
            "/skills/bilibili-subtitle/credentials",
            get(poll_bilibili_credentials).post(start_bilibili_credentials),
        )
        .route("/skills/{skill_name}/files", get(skill_files))
        .route(
            "/skills/{skill_name}/file",
            get(get_skill_file).put(put_skill_file),
        )
}

async fn admin_skills(State(state): State<Arc<AdminState>>) -> Result<Json<Value>, ApiError> {
    let app_config = state.app_config();
    let enabled = app_config.app.enabled_skills.clone();
    let root = PathBuf::from(crate::skills::DIRECTORY_NAME);
    let normalized = tokio::task::spawn_blocking({
        let root = root.clone();
        let enabled = enabled.clone();
        move || normalize_enabled_names(&root, enabled)
    })
    .await??;
    if normalized != enabled {
        let config_path = state.config_path.clone();
        let normalized = normalized.clone();
        tokio::task::spawn_blocking(move || {
            super::config_store::write_enabled_skills(&config_path, &normalized)
        })
        .await??;
    }
    let items = tokio::task::spawn_blocking(move || list_skills(&root, &normalized)).await??;
    let environment = app_config
        .skill_environment_groups
        .iter()
        .flat_map(|(skill, variables)| {
            variables.iter().map(move |(name, value)| {
                json!({ "skill": skill, "name": name, "configured": !value.is_empty() })
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "items": items, "environment": environment })))
}

#[derive(Deserialize)]
struct SkillSettingsUpdate {
    enabled: Vec<String>,
    environment: Vec<SkillEnvironmentUpdate>,
}

#[derive(Deserialize)]
struct SkillEnvironmentUpdate {
    skill: String,
    name: String,
    #[serde(default)]
    value: Option<String>,
}

async fn put_skill_settings(
    State(state): State<Arc<AdminState>>,
    Json(update): Json<SkillSettingsUpdate>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let app_config = state.app_config();
    let mut environment = SkillEnvironmentGroups::new();
    let mut names = HashSet::new();
    for variable in update.environment {
        let skill = variable.skill.trim().to_string();
        let name = variable.name.trim().to_string();
        if !names.insert(name.clone()) {
            return Err(ApiError::bad_request(format!(
                "Skill 环境变量名称重复：{}",
                name
            )));
        }
        // 空值表示保留现有密钥；新变量必须明确提供值，API 始终不会回显旧值。
        let value = variable
            .value
            .filter(|value| !value.is_empty())
            .or_else(|| {
                app_config
                    .skill_environment_groups
                    .get(&skill)
                    .and_then(|variables| variables.get(&name))
                    .cloned()
            })
            .ok_or_else(|| ApiError::bad_request(format!("请填写环境变量 {} 的值", name)))?;
        environment.entry(skill).or_default().insert(name, value);
    }
    validate_skill_environment_groups(&environment)
        .map_err(|error| ApiError::bad_request(format!("{error:#}")))?;

    let root = PathBuf::from(crate::skills::DIRECTORY_NAME);
    let enabled =
        tokio::task::spawn_blocking(move || normalize_enabled_names(&root, update.enabled))
            .await??;
    let config_path = state.config_path.clone();
    let saved_enabled = enabled.clone();
    let config = tokio::task::spawn_blocking(move || {
        super::config_store::write_skill_settings(&config_path, &saved_enabled, &environment)
    })
    .await??;
    state.replace_app_config(config);
    schedule_restart(&state.restart);
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "enabled": enabled, "restarting": true })),
    ))
}

async fn start_bilibili_credentials() -> Result<Json<Value>, ApiError> {
    Ok(Json(super::bilibili_auth::run_login_action("start").await?))
}

async fn poll_bilibili_credentials() -> Result<Json<Value>, ApiError> {
    Ok(Json(super::bilibili_auth::run_login_action("poll").await?))
}

async fn skill_files(Path(skill_name): Path<String>) -> Result<Json<Value>, ApiError> {
    let root = PathBuf::from(crate::skills::DIRECTORY_NAME);
    let files = tokio::task::spawn_blocking(move || list_skill_files(&root, &skill_name)).await??;
    Ok(Json(json!({ "items": files })))
}

#[derive(Deserialize)]
struct SkillFileQuery {
    path: String,
}

async fn get_skill_file(
    Path(skill_name): Path<String>,
    Query(query): Query<SkillFileQuery>,
) -> Result<Json<Value>, ApiError> {
    let root = PathBuf::from(crate::skills::DIRECTORY_NAME);
    let path = query.path;
    let response_path = path.clone();
    let content =
        tokio::task::spawn_blocking(move || read_skill_file(&root, &skill_name, &path)).await??;
    Ok(Json(json!({ "path": response_path, "content": content })))
}

#[derive(Deserialize)]
struct SkillFileUpdate {
    content: String,
}

async fn put_skill_file(
    Path(skill_name): Path<String>,
    Query(query): Query<SkillFileQuery>,
    Json(update): Json<SkillFileUpdate>,
) -> Result<Json<Value>, ApiError> {
    let root = PathBuf::from(crate::skills::DIRECTORY_NAME);
    let path = query.path;
    tokio::task::spawn_blocking(move || {
        write_skill_file(&root, &skill_name, &path, &update.content)
    })
    .await??;
    Ok(Json(json!({ "saved": true })))
}
