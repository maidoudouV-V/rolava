use std::collections::HashMap;
use std::fs;
use anyhow::Result;
use serde::Deserialize;
use crate::ai_provider::{
    AIProvider,
    anthropic::AnthropicProvider,
    openai_compatible::OpenAICompatibleProvider,
    openrouter::OpenRouterProvider,
};

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
    /// 默认聊天模型名称
    pub chat_model_name: String,
    /// 默认 Agent 模型名称
    pub agent_model_name: String,
    /// 启用的工具列表
    pub tools: Vec<String>,
    /// 私聊白名单 QQ 号，空数组表示放行所有私聊。
    pub direct_whitelist: Vec<String>,
    /// 群聊白名单群号，空数组表示放行所有群聊。
    pub group_whitelist: Vec<String>,
    /// 模拟回复时每个字符对应的最少等待秒数。
    pub reply_delay_per_char_secs: f64,
    /// 模拟回复时额外随机等待的最大秒数。
    pub reply_delay_random_max_secs: f64,
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
            let provider: Box<dyn AIProvider + Send + Sync> = match provider_config.r#type.as_str() {
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
                _ => return Err(anyhow::anyhow!("Unsupported provider type: {}", provider_config.r#type)),
            };
            ai_providers.insert(provider_config.name.clone(), provider);
        }
        let prompt_config = PromptConfig::new(&toml_config.app)?;
        Ok(AppConfig {
            app: toml_config.app,
            server: toml_config.server,
            ai_providers,
            prompt_config
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
        let new_config = Self {
            system_prompt: fs::read_to_string(format!("{}/system.md", app.prompt_dir))?,
            character_prompt: fs::read_to_string(format!("{}/character.md", app.prompt_dir))?,
            instruction_prompt: fs::read_to_string(format!("{}/instruction.md", app.prompt_dir))?,
        };
        Ok(new_config)
    }
}
