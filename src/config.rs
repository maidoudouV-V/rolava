use crate::ai_provider::{
    google_aistudio::GoogleAIStudioProvider, openai_compatible::OpenAICompatibleProvider,
    openrouter::OpenRouterProvider, AIProvider,
};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

const DEFAULT_AI_REQUEST_RETRY_COUNT: u32 = 1;
const DEFAULT_AI_REQUEST_TIMEOUT_SECONDS: u64 = 0;
const DEFAULT_VISION_IMAGE_MESSAGE_WINDOW: usize = 10;

#[derive(Deserialize, Debug)]
struct TomlConfig {
    /// 应用相关配置
    app: AppSection,
    /// 服务监听与 OneBot 通信配置
    server: ServerSection,
    /// AI 服务商配置列表
    providers: Vec<ProviderConfig>,
    /// AI 模型配置列表
    models: Vec<ModelConfig>,
    /// 日志配置。
    #[serde(default)]
    logging: LoggingSection,
}

#[derive(Deserialize, Debug, Clone, Copy, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

#[derive(Deserialize, Debug, Default)]
pub struct LoggingSection {
    /// 应用日志级别，可选 error、warn、info、debug 或 trace。
    #[serde(default)]
    pub level: LogLevel,
}

#[derive(Deserialize, Debug)]
pub struct AppSection {
    /// 模板目录路径
    pub prompt_dir: String,
    /// 发送给模型的最大历史消息数。
    pub max_history_messages: u32,
    /// 主模型请求中允许附带原始图片的最近正文消息数；0 表示不附带原图。
    #[serde(default = "default_vision_image_message_window")]
    pub vision_image_message_window: usize,
    /// 更主动回复的概率，范围 0-100；保留供后续流程使用。
    pub proactive_reply_percent: f64,
    /// 默认聊天模型名称
    pub chat_model_name: String,
    /// 消息过滤模型名称。
    pub filter_model_name: String,
    /// 是否启用 AI 前置消息过滤。
    #[serde(default = "default_enable_ai_filter")]
    pub enable_ai_filter: bool,
    /// 联网搜索模型名称。
    pub web_search_model_name: String,
    /// 后台图片描述模型名称；为空时关闭图片描述功能。
    pub visual_model_name: String,
    /// AI 请求失败后的额外重试次数；1 表示最多尝试 2 次。
    #[serde(default = "default_ai_request_retry_count")]
    pub ai_request_retry_count: u32,
    /// AI 单次请求总超时时间，单位秒；0 表示不设置超时。
    #[serde(default = "default_ai_request_timeout_seconds")]
    pub ai_request_timeout_seconds: u64,
    /// 接收到的图片本地保存目录。
    pub received_image_dir: String,
    /// 启用的可选动作列表；会话控制工具固定启用。
    pub enabled_actions: Vec<String>,
    /// 私聊白名单 QQ 号，空数组表示放行所有私聊。
    pub direct_whitelist: Vec<String>,
    /// 群聊白名单群号，空数组表示放行所有群聊。
    pub group_whitelist: Vec<String>,
    /// 命令执行账号白名单；空数组表示所有账号都不能执行命令。
    #[serde(default)]
    pub command_whitelist: Vec<String>,
    /// 是否按换行符拆分并逐段发送回复文本。
    pub split_reply_on_newlines: bool,
    /// 模拟回复时总随机等待的最大秒数。
    pub reply_delay_random_max_secs: f64,
}

impl AppSection {
    pub fn ai_request_max_attempts(&self) -> u32 {
        self.ai_request_retry_count.saturating_add(1).max(1)
    }
}

fn default_ai_request_retry_count() -> u32 {
    DEFAULT_AI_REQUEST_RETRY_COUNT
}

fn default_ai_request_timeout_seconds() -> u64 {
    DEFAULT_AI_REQUEST_TIMEOUT_SECONDS
}

fn default_vision_image_message_window() -> usize {
    DEFAULT_VISION_IMAGE_MESSAGE_WINDOW
}

fn default_enable_ai_filter() -> bool {
    true
}

#[derive(Deserialize, Debug)]
pub struct ServerSection {
    /// 本服务监听地址
    pub server_host: String,
    /// 本服务监听端口
    pub server_port: u16,
    /// 本服务 访问密钥
    pub server_token: String,
    /// OneBot 服务地址
    pub onebot_api: String,
    /// OneBot 访问密钥
    pub onebot_token: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ProviderConfig {
    /// 服务商名称，用作唯一标识
    pub name: String,
    /// 服务商类型，如 openai_compatible、google_aistudio 或 openrouter
    pub r#type: String,
    /// 服务商访问密钥
    pub key: String,
    /// 服务商接口基础地址
    pub base_url: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ModelConfig {
    /// 模型配置名称，用作应用内唯一标识
    pub name: String,
    /// 模型使用的服务商配置名称
    pub provider: String,
    /// 远端 API 接受的模型 ID
    pub model: String,
    /// 最大输出 token 数。
    pub max_tokens: i32,
    /// 推理强度，必填，如 none、minimal、low、medium、high、xhigh
    pub reasoning_effort: String,
    /// 是否启用模型的图像输入能力；未配置时默认禁用。
    #[serde(default)]
    pub vision: ModelFeatureState,
}

#[derive(Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModelFeatureState {
    Enable,
    #[default]
    Disable,
}

impl ModelFeatureState {
    pub fn is_enabled(self) -> bool {
        self == Self::Enable
    }
}

/// 模型配置声明的输入与工具能力，不代表 Provider 会自动探测能力。
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelCapabilities {
    pub vision: ModelFeatureState,
}

pub struct AppConfig {
    /// 应用相关配置
    pub app: AppSection,
    /// 服务监听与 OneBot 通信配置
    pub server: ServerSection,
    /// 按模型配置名称索引的 AI 模型实例
    pub ai_models: HashMap<String, Box<dyn AIProvider + Send + Sync>>,
    /// 按模型配置名称索引的能力声明。
    pub model_capabilities: HashMap<String, ModelCapabilities>,
    /// 提示词配置
    pub prompt_config: PromptConfig,
    /// 日志输出配置。
    pub logging: LoggingSection,
    /// QQ 经典表情 ID 到名称的映射。
    pub face_id_map: HashMap<String, String>,
}

impl AppConfig {
    pub fn new(config_path: &str) -> Result<Self> {
        let config_path = Path::new(config_path);
        let toml_str = std::fs::read_to_string(config_path)?;
        let TomlConfig {
            app,
            server,
            providers,
            models,
            logging,
        } = toml::from_str(&toml_str)?;

        let mut provider_configs = HashMap::new();
        for provider_config in providers {
            let provider_name = provider_config.name.clone();
            if provider_configs
                .insert(provider_name.clone(), provider_config)
                .is_some()
            {
                anyhow::bail!("服务商配置名称重复：{}", provider_name);
            }
        }

        let mut ai_models = HashMap::<String, Box<dyn AIProvider + Send + Sync>>::new();
        let mut model_capabilities = HashMap::new();
        for model_config in models {
            model_capabilities.insert(
                model_config.name.clone(),
                ModelCapabilities {
                    vision: model_config.vision,
                },
            );
            let provider_config =
                provider_configs
                    .get(&model_config.provider)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "模型配置 {} 引用了不存在的服务商：{}",
                            model_config.name,
                            model_config.provider
                        )
                    })?;
            let model: Box<dyn AIProvider + Send + Sync> = match provider_config.r#type.as_str() {
                "openai_compatible" => Box::new(OpenAICompatibleProvider::new(
                    provider_config.key.clone(),
                    provider_config.base_url.clone(),
                    model_config.model,
                    model_config.max_tokens,
                    model_config.reasoning_effort,
                )),
                "openrouter" => Box::new(OpenRouterProvider::new(
                    provider_config.key.clone(),
                    provider_config.base_url.clone(),
                    model_config.model,
                    model_config.max_tokens,
                    model_config.reasoning_effort,
                )),
                "google_aistudio" => Box::new(GoogleAIStudioProvider::new(
                    provider_config.key.clone(),
                    provider_config.base_url.clone(),
                    model_config.model,
                    model_config.max_tokens,
                )),
                _ => {
                    return Err(anyhow::anyhow!(
                        "不支持的服务商类型：{}",
                        provider_config.r#type
                    ))
                }
            };
            let model_name = model_config.name;
            if ai_models.insert(model_name.clone(), model).is_some() {
                anyhow::bail!("模型配置名称重复：{}", model_name);
            }
        }

        for (purpose, model_name) in [
            ("聊天", &app.chat_model_name),
            ("消息过滤", &app.filter_model_name),
            ("联网搜索", &app.web_search_model_name),
            ("图像识别", &app.visual_model_name),
        ] {
            if !model_name.trim().is_empty() && !ai_models.contains_key(model_name) {
                anyhow::bail!("{}模型配置不存在：{}", purpose, model_name);
            }
        }

        let prompt_config = PromptConfig::new(&app)?;
        // 表情映射与主配置放在同一目录，启动时加载一次供所有消息转换复用。
        let face_id_map_path = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("face_id_map.json");
        let face_id_map_text = fs::read_to_string(&face_id_map_path).with_context(|| {
            format!(
                "读取 QQ 表情映射失败：{}",
                face_id_map_path.to_string_lossy()
            )
        })?;
        let face_id_map = serde_json::from_str(&face_id_map_text).with_context(|| {
            format!(
                "解析 QQ 表情映射失败：{}",
                face_id_map_path.to_string_lossy()
            )
        })?;
        Ok(AppConfig {
            app,
            server,
            ai_models,
            model_capabilities,
            prompt_config,
            logging,
            face_id_map,
        })
    }

    pub fn chat_model_supports_vision(&self) -> bool {
        self.model_capabilities
            .get(&self.app.chat_model_name)
            .is_some_and(|capabilities| capabilities.vision.is_enabled())
    }
}

pub struct PromptConfig {
    pub system_prompt: String,
    pub character_prompt: String,
    pub instruction_prompt: String,
    pub filter_prompt: String,
}
impl PromptConfig {
    pub fn new(app: &AppSection) -> Result<Self> {
        let prompt_dir = Path::new(&app.prompt_dir);
        let system_template = fs::read_to_string(prompt_dir.join("system.md"))?;
        let enabled_actions_prompt = Self::load_enabled_action_prompts(app, prompt_dir)?;
        let character_prompt = fs::read_to_string(prompt_dir.join("character.md"))?;
        let filter_template = fs::read_to_string(prompt_dir.join("filter.md"))?;
        let new_config = Self {
            system_prompt: system_template
                .replace("{{enabled_actions}}", enabled_actions_prompt.trim()),
            character_prompt: character_prompt.clone(),
            instruction_prompt: fs::read_to_string(prompt_dir.join("instruction.md"))?,
            filter_prompt: filter_template.replace("{{character_prompt}}", character_prompt.trim()),
        };
        Ok(new_config)
    }

    /// 按配置读取可选动作提示词，文件名必须与动作名一致。
    fn load_enabled_action_prompts(app: &AppSection, prompt_dir: &Path) -> Result<String> {
        let mut action_prompts = Vec::new();
        for action_name in &app.enabled_actions {
            let action_name = action_name.trim();
            if action_name.is_empty() {
                continue;
            }
            if action_name.contains('/') || action_name.contains('\\') || action_name.contains("..")
            {
                anyhow::bail!("可选动作名称不合法：{}", action_name);
            }

            let action_prompt_path = prompt_dir
                .join("actions")
                .join(format!("{}.md", action_name));
            let action_prompt = fs::read_to_string(&action_prompt_path).with_context(|| {
                format!(
                    "读取可选动作提示词失败：{}",
                    action_prompt_path.to_string_lossy()
                )
            })?;
            action_prompts.push(action_prompt.trim().to_string());
        }

        Ok(action_prompts.join("\n\n"))
    }
}
