use crate::ai_provider::{
    anthropic::AnthropicProvider, google_aistudio::GoogleAIStudioProvider,
    openai_compatible::OpenAICompatibleProvider, openrouter::OpenRouterProvider, AIProvider,
};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

const DEFAULT_AI_REQUEST_RETRY_COUNT: u32 = 1;
const DEFAULT_AI_REQUEST_TIMEOUT_SECONDS: u64 = 0;

#[derive(Deserialize, Debug)]
struct TomlConfig {
    /// 应用相关配置
    app: AppSection,
    /// 服务监听与 OneBot 通信配置
    server: ServerSection,
    /// AI 服务商配置列表
    providers: Vec<ProviderConfig>,
}

#[derive(Deserialize, Debug)]
pub struct AppSection {
    /// 模板目录路径
    pub prompt_dir: String,
    /// 发送给模型的最大历史消息数。
    pub max_history_messages: u32,
    /// 当前回复决策状态为“更主动”的概率，范围 0-100。
    pub proactive_reply_percent: f64,
    /// 默认聊天模型名称
    pub chat_model_name: String,
    /// 联网搜索模型名称。
    pub web_search_model_name: String,
    /// 默认视觉模型名称，用于图片识别等消息增强流程。
    pub visual_model_name: String,
    /// AI 请求失败后的额外重试次数；1 表示最多尝试 2 次。
    #[serde(default = "default_ai_request_retry_count")]
    pub ai_request_retry_count: u32,
    /// AI 单次请求总超时时间，单位秒；0 表示不设置超时。
    #[serde(default = "default_ai_request_timeout_seconds")]
    pub ai_request_timeout_seconds: u64,
    /// 接收到的图片本地保存目录。
    pub received_image_dir: String,
    /// 启用的可选动作列表，send_message、wait_then_check、ignore_messages 固定启用。
    pub enabled_actions: Vec<String>,
    /// 私聊白名单 QQ 号，空数组表示放行所有私聊。
    pub direct_whitelist: Vec<String>,
    /// 群聊白名单群号，空数组表示放行所有群聊。
    pub group_whitelist: Vec<String>,
    /// 模拟回复时每个字符对应的最少等待秒数。
    pub reply_delay_per_char_secs: f64,
    /// 模拟回复时额外随机等待的最大秒数。
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
    /// 服务商类型，如 openai_compatible、anthropic 或 openrouter
    pub r#type: String,
    /// 服务商访问密钥
    pub key: String,
    /// 服务商接口基础地址
    pub base_url: String,
    /// 服务商默认模型名称
    pub model: String,
    /// 最大输出 token 数。
    pub max_tokens: i32,
    /// 推理强度，必填，如 none、minimal、low、medium、high、xhigh
    pub reasoning_effort: String,
}

pub struct AppConfig {
    /// 应用相关配置
    pub app: AppSection,
    /// 服务监听与 OneBot 通信配置
    pub server: ServerSection,
    /// 已初始化的 AI 服务商实例列表
    pub ai_providers: HashMap<String, Box<dyn AIProvider + Send + Sync>>,
    /// 提示词配置
    pub prompt_config: PromptConfig,
}

impl AppConfig {
    pub fn new(config_path: &str) -> Result<Self> {
        let toml_str = std::fs::read_to_string(config_path)?;
        let toml_config: TomlConfig = toml::from_str(&toml_str)?;

        let mut ai_providers = HashMap::<String, Box<dyn AIProvider + Send + Sync>>::new();
        for provider_config in toml_config.providers {
            let provider: Box<dyn AIProvider + Send + Sync> = match provider_config.r#type.as_str()
            {
                "openai_compatible" => Box::new(OpenAICompatibleProvider::new(
                    provider_config.key,
                    provider_config.base_url,
                    provider_config.model,
                    provider_config.max_tokens,
                    provider_config.reasoning_effort,
                )),
                "anthropic" => Box::new(AnthropicProvider::new(
                    provider_config.key,
                    provider_config.base_url,
                    provider_config.model,
                )),
                "openrouter" => Box::new(OpenRouterProvider::new(
                    provider_config.key,
                    provider_config.base_url,
                    provider_config.model,
                    provider_config.max_tokens,
                    provider_config.reasoning_effort,
                )),
                "google_aistudio" => Box::new(GoogleAIStudioProvider::new(
                    provider_config.key,
                    provider_config.base_url,
                    provider_config.model,
                    provider_config.max_tokens,
                )),
                _ => {
                    return Err(anyhow::anyhow!(
                        "不支持的服务商类型：{}",
                        provider_config.r#type
                    ))
                }
            };
            ai_providers.insert(provider_config.name.clone(), provider);
        }
        let prompt_config = PromptConfig::new(&toml_config.app)?;
        Ok(AppConfig {
            app: toml_config.app,
            server: toml_config.server,
            ai_providers,
            prompt_config,
        })
    }
}

pub struct PromptConfig {
    pub system_prompt: String,
    pub character_prompt: String,
    pub instruction_prompt: String,
}
impl PromptConfig {
    pub fn new(app: &AppSection) -> Result<Self> {
        let prompt_dir = Path::new(&app.prompt_dir);
        let system_template = fs::read_to_string(prompt_dir.join("system.md"))?;
        let enabled_actions_prompt = Self::load_enabled_action_prompts(app, prompt_dir)?;
        let new_config = Self {
            system_prompt: system_template
                .replace("{{enabled_actions}}", enabled_actions_prompt.trim()),
            character_prompt: fs::read_to_string(prompt_dir.join("character.md"))?,
            instruction_prompt: fs::read_to_string(prompt_dir.join("instruction.md"))?,
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
