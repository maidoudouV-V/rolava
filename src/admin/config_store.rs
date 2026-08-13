use crate::config::{AppConfig, ModelConfig};
use crate::tools::ToolRegistry;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Item, Table};

#[derive(Debug, Serialize)]
pub struct AdminConfigView {
    pub app: AdminAppConfig,
    pub server: AdminServerConfig,
    pub providers: Vec<AdminProviderConfig>,
    pub models: Vec<ModelConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdminAppConfig {
    pub chat_model_name: String,
    pub filter_model_name: String,
    pub web_search_model_name: String,
    pub visual_model_name: String,
    pub max_history_messages: u32,
    pub vision_image_message_window: usize,
    pub ai_request_retry_count: u32,
    pub ai_request_timeout_seconds: u64,
    pub enabled_actions: Vec<String>,
    pub direct_whitelist: Vec<String>,
    pub group_whitelist: Vec<String>,
    pub command_whitelist: Vec<String>,
    pub reply_delay_random_max_secs: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdminServerConfig {
    pub server_host: String,
    pub server_port: u16,
    pub server_token_configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_token: Option<String>,
    pub onebot_api: String,
    pub onebot_token_configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onebot_token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdminProviderConfig {
    pub name: String,
    /// 用于重命名时保留原有密钥，不参与 Provider 运行配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_name: Option<String>,
    pub r#type: String,
    pub base_url: String,
    pub key_configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AdminConfigUpdate {
    pub app: AdminAppConfig,
    pub server: AdminServerConfig,
    pub providers: Vec<AdminProviderConfig>,
    pub models: Vec<ModelConfig>,
}

impl AdminConfigView {
    pub fn from_config(config: &AppConfig) -> Self {
        Self {
            app: AdminAppConfig {
                chat_model_name: config.app.chat_model_name.clone(),
                filter_model_name: config.app.filter_model_name.clone(),
                web_search_model_name: config.app.web_search_model_name.clone(),
                visual_model_name: config.app.visual_model_name.clone(),
                max_history_messages: config.app.max_history_messages,
                vision_image_message_window: config.app.vision_image_message_window,
                ai_request_retry_count: config.app.ai_request_retry_count,
                ai_request_timeout_seconds: config.app.ai_request_timeout_seconds,
                enabled_actions: config.app.enabled_actions.clone(),
                direct_whitelist: config.app.direct_whitelist.clone(),
                group_whitelist: config.app.group_whitelist.clone(),
                command_whitelist: config.app.command_whitelist.clone(),
                reply_delay_random_max_secs: config.app.reply_delay_random_max_secs,
            },
            server: AdminServerConfig {
                server_host: config.server.server_host.clone(),
                server_port: config.server.server_port,
                server_token_configured: !config.server.server_token.is_empty(),
                server_token: None,
                onebot_api: config.server.onebot_api.clone(),
                onebot_token_configured: !config.server.onebot_token.is_empty(),
                onebot_token: None,
            },
            providers: config
                .providers
                .iter()
                .map(|provider| AdminProviderConfig {
                    name: provider.name.clone(),
                    original_name: Some(provider.name.clone()),
                    r#type: provider.r#type.clone(),
                    base_url: provider.base_url.clone(),
                    key_configured: !provider.key.is_empty(),
                    key: None,
                })
                .collect(),
            models: config.models.clone(),
        }
    }
}

/// 确保管理 Token 存在；只在首次生成时改写配置文件。
pub fn ensure_admin_token(config_path: &Path) -> Result<String> {
    let source = fs::read_to_string(config_path)
        .with_context(|| format!("读取配置失败：{}", config_path.display()))?;
    let mut document = source
        .parse::<DocumentMut>()
        .context("解析配置 TOML 失败")?;
    let existing = document
        .get("admin")
        .and_then(Item::as_table)
        .and_then(|table| table.get("token"))
        .and_then(Item::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if !existing.is_empty() {
        return Ok(existing);
    }

    let mut random = [0_u8; 32];
    OsRng.fill_bytes(&mut random);
    let token = URL_SAFE_NO_PAD.encode(random);
    document["admin"]["token"] = value(&token);
    persist_document(config_path, &document)?;
    Ok(token)
}

pub fn write_admin_config(config_path: &Path, update: AdminConfigUpdate) -> Result<()> {
    validate_update(&update)?;
    let source = fs::read_to_string(config_path)?;
    let mut document = source
        .parse::<DocumentMut>()
        .context("解析配置 TOML 失败")?;
    apply_app(&mut document, &update.app);
    apply_server(&mut document, &update.server);
    apply_providers(&mut document, &update.providers);
    apply_models(&mut document, &update.models);

    // 候选配置先写入同目录临时文件并完整加载，所有引用和提示词均通过后才替换原文件。
    let mut candidate =
        NamedTempFile::new_in(config_path.parent().unwrap_or_else(|| Path::new(".")))?;
    candidate.write_all(document.to_string().as_bytes())?;
    candidate.flush()?;
    AppConfig::new(candidate.path().to_string_lossy().as_ref()).context("新配置校验失败")?;
    candidate
        .persist(config_path)
        .map_err(|error| error.error)?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct PromptFileSummary {
    pub id: String,
    pub name: String,
    pub category: &'static str,
}

const INTERNAL_PROMPT_FILES: &[&str] = &[
    "image_description",
    "web_search_agent",
    "scheduled_task",
    "scheduled_task_recovery",
    "wait_for_reply_timeout",
];

pub fn list_prompt_files(config: &AppConfig) -> Result<Vec<PromptFileSummary>> {
    let mut prompts = vec![
        PromptFileSummary {
            id: "system".into(),
            name: "system.md".into(),
            category: "core",
        },
        PromptFileSummary {
            id: "character".into(),
            name: "character.md".into(),
            category: "core",
        },
        PromptFileSummary {
            id: "instruction".into(),
            name: "instruction.md".into(),
            category: "core",
        },
        PromptFileSummary {
            id: "filter".into(),
            name: "filter.md".into(),
            category: "core",
        },
    ];
    prompts.extend(INTERNAL_PROMPT_FILES.iter().map(|name| PromptFileSummary {
        id: format!("internal:{}", name),
        name: format!("{}.md", name),
        category: "internal",
    }));
    let actions_dir = Path::new(&config.app.prompt_dir).join("actions");
    if actions_dir.exists() {
        let mut actions = fs::read_dir(actions_dir)?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension().and_then(|value| value.to_str()) == Some("md"))
                    .then(|| path.file_stem()?.to_str().map(str::to_string))?
            })
            .collect::<Vec<_>>();
        actions.sort();
        prompts.extend(actions.into_iter().map(|name| PromptFileSummary {
            id: format!("action:{}", name),
            name: format!("{}.md", name),
            category: "action",
        }));
    }
    Ok(prompts)
}

pub fn read_prompt(config: &AppConfig, prompt_id: &str) -> Result<String> {
    fs::read_to_string(resolve_prompt_path(config, prompt_id)?)
        .with_context(|| format!("读取提示词 {} 失败", prompt_id))
}

pub fn write_prompt(config: &AppConfig, prompt_id: &str, content: &str) -> Result<()> {
    if content.trim().is_empty() {
        anyhow::bail!("提示词不能为空");
    }
    let path = resolve_prompt_path(config, prompt_id)?;
    let mut file = NamedTempFile::new_in(path.parent().unwrap_or_else(|| Path::new(".")))?;
    file.write_all(content.as_bytes())?;
    file.flush()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn resolve_prompt_path(config: &AppConfig, prompt_id: &str) -> Result<PathBuf> {
    let root = Path::new(&config.app.prompt_dir);
    let path = match prompt_id {
        "system" | "character" | "instruction" | "filter" => root.join(format!("{}.md", prompt_id)),
        _ if prompt_id.starts_with("internal:") => {
            let name = prompt_id.trim_start_matches("internal:");
            if !INTERNAL_PROMPT_FILES.contains(&name) {
                anyhow::bail!("未知运行时提示词 {}", prompt_id);
            }
            root.join("internal").join(format!("{}.md", name))
        }
        _ => {
            let name = prompt_id
                .strip_prefix("action:")
                .filter(|name| !name.is_empty())
                .ok_or_else(|| anyhow::anyhow!("未知提示词 {}", prompt_id))?;
            if !name.chars().all(|character| {
                character.is_ascii_alphanumeric() || character == '_' || character == '-'
            }) {
                anyhow::bail!("动作提示词名称不合法");
            }
            root.join("actions").join(format!("{}.md", name))
        }
    };
    if !path.is_file() {
        anyhow::bail!("提示词不存在：{}", prompt_id);
    }
    Ok(path)
}

fn validate_update(update: &AdminConfigUpdate) -> Result<()> {
    if update.app.max_history_messages == 0 {
        anyhow::bail!("历史消息数必须大于 0");
    }
    if !update.app.reply_delay_random_max_secs.is_finite()
        || update.app.reply_delay_random_max_secs < 0.0
    {
        anyhow::bail!("随机回复延时必须是非负数");
    }
    if update.server.server_host.trim().is_empty() || update.server.onebot_api.trim().is_empty() {
        anyhow::bail!("监听地址和 OneBot API 不能为空");
    }
    let enabled_tools = update
        .app
        .enabled_actions
        .iter()
        .map(|name| name.trim())
        .collect::<std::collections::HashSet<_>>();
    if enabled_tools.len() != update.app.enabled_actions.len() {
        anyhow::bail!("启用的可选工具不能重复");
    }
    if let Some(name) = enabled_tools
        .iter()
        .find(|name| !ToolRegistry::is_optional_tool(name))
    {
        anyhow::bail!("未知的可选工具：{}", name);
    }
    if enabled_tools.contains("agent_web_search")
        && update.app.web_search_model_name.trim().is_empty()
    {
        anyhow::bail!("启用网络搜索前必须选择联网搜索模型");
    }
    let provider_names = update
        .providers
        .iter()
        .map(|provider| provider.name.trim())
        .collect::<std::collections::HashSet<_>>();
    if provider_names.len() != update.providers.len() || provider_names.contains("") {
        anyhow::bail!("Provider 名称不能为空或重复");
    }
    let model_names = update
        .models
        .iter()
        .map(|model| model.name.trim())
        .collect::<std::collections::HashSet<_>>();
    if model_names.len() != update.models.len() || model_names.contains("") {
        anyhow::bail!("模型名称不能为空或重复");
    }
    for model in &update.models {
        if !provider_names.contains(model.provider.trim()) {
            anyhow::bail!("模型 {} 引用了不存在的 Provider", model.name);
        }
        if model.max_tokens.is_some_and(|max_tokens| max_tokens <= 0) {
            anyhow::bail!("模型 {} 的最大输出长度必须大于 0", model.name);
        }
    }
    Ok(())
}

fn apply_app(document: &mut DocumentMut, app: &AdminAppConfig) {
    let table = &mut document["app"];
    table["chat_model_name"] = value(&app.chat_model_name);
    table["filter_model_name"] = value(&app.filter_model_name);
    table["web_search_model_name"] = value(&app.web_search_model_name);
    table["visual_model_name"] = value(&app.visual_model_name);
    table["max_history_messages"] = value(i64::from(app.max_history_messages));
    table["vision_image_message_window"] = value(app.vision_image_message_window as i64);
    table["ai_request_retry_count"] = value(i64::from(app.ai_request_retry_count));
    table["ai_request_timeout_seconds"] = value(app.ai_request_timeout_seconds as i64);
    table["enabled_actions"] = Item::Value(strings_array(&app.enabled_actions).into());
    table["direct_whitelist"] = Item::Value(strings_array(&app.direct_whitelist).into());
    table["group_whitelist"] = Item::Value(strings_array(&app.group_whitelist).into());
    table["command_whitelist"] = Item::Value(strings_array(&app.command_whitelist).into());
    table["reply_delay_random_max_secs"] = value(app.reply_delay_random_max_secs);
    table.as_table_mut().map(|table| {
        table.remove("split_reply_on_newlines");
        table.remove("enable_ai_filter");
    });
}

fn apply_server(document: &mut DocumentMut, server: &AdminServerConfig) {
    let table = &mut document["server"];
    table["server_host"] = value(server.server_host.trim());
    table["server_port"] = value(i64::from(server.server_port));
    table["onebot_api"] = value(server.onebot_api.trim().trim_end_matches('/'));
    if let Some(token) = server
        .server_token
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        table["server_token"] = value(token);
    }
    if let Some(token) = server
        .onebot_token
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        table["onebot_token"] = value(token);
    }
}

fn apply_providers(document: &mut DocumentMut, providers: &[AdminProviderConfig]) {
    let current = document
        .get("providers")
        .and_then(Item::as_array_of_tables)
        .map(|tables| {
            tables
                .iter()
                .filter_map(|table| {
                    Some((
                        table.get("name")?.as_str()?.to_string(),
                        table.get("key")?.as_str()?.to_string(),
                    ))
                })
                .collect::<std::collections::HashMap<_, _>>()
        })
        .unwrap_or_default();
    let mut tables = ArrayOfTables::new();
    for provider in providers {
        let mut table = Table::new();
        table["name"] = value(provider.name.trim());
        table["type"] = value(provider.r#type.trim());
        table["key"] = value(
            provider
                .key
                .as_deref()
                .filter(|key| !key.is_empty())
                .or_else(|| current.get(provider.name.trim()).map(String::as_str))
                .or_else(|| {
                    provider
                        .original_name
                        .as_deref()
                        .and_then(|name| current.get(name).map(String::as_str))
                })
                .unwrap_or_default(),
        );
        table["base_url"] = value(provider.base_url.trim().trim_end_matches('/'));
        tables.push(table);
    }
    document["providers"] = Item::ArrayOfTables(tables);
}

fn apply_models(document: &mut DocumentMut, models: &[ModelConfig]) {
    let mut tables = ArrayOfTables::new();
    for model in models {
        let mut table = Table::new();
        table["name"] = value(model.name.trim());
        table["provider"] = value(model.provider.trim());
        table["model"] = value(model.model.trim());
        if let Some(max_tokens) = model.max_tokens {
            table["max_tokens"] = value(i64::from(max_tokens));
        }
        table["reasoning_effort"] = value(model.reasoning_effort.trim());
        table["vision"] = value(model.vision.as_str());
        tables.push(table);
    }
    document["models"] = Item::ArrayOfTables(tables);
}

fn strings_array(values: &[String]) -> Array {
    let mut array = Array::new();
    for value in values {
        array.push(value.as_str());
    }
    array
}

fn persist_document(path: &Path, document: &DocumentMut) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut file = NamedTempFile::new_in(parent)?;
    file.write_all(document.to_string().as_bytes())?;
    file.flush()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

pub fn config_path() -> PathBuf {
    PathBuf::from("config/meta.toml")
}

#[cfg(test)]
mod tests {
    use super::ensure_admin_token;
    use std::fs;

    #[test]
    fn empty_admin_token_is_generated_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("meta.toml");
        fs::write(&path, "[admin]\ntoken = \"\"\n").unwrap();

        let generated = ensure_admin_token(&path).unwrap();
        let loaded_again = ensure_admin_token(&path).unwrap();

        assert_eq!(generated, loaded_again);
        assert!(generated.len() >= 40);
        assert!(fs::read_to_string(path).unwrap().contains(&generated));
    }
}
