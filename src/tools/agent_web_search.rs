use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::{timeout, Duration};
use tracing::warn;

use crate::config::render_prompt_template;

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"启动一个新的互联网 Agent 查询内容。
输入你的问题，此工具会启用子 Agent 通过互联网查询并解决你的问题。
当需要获取互联网实时信息时调用，工具暂时不提供图片搜索功能。
请注意调用频率，只有确实需要连接互联网才能提供回答时才可使用。"#;

#[derive(Debug, Deserialize)]
pub struct AgentWebSearchArgs {
    pub question: String,
}

pub struct AgentWebSearchTool;

#[async_trait]
impl Tool for AgentWebSearchTool {
    fn name(&self) -> &'static str {
        "agent_web_search"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "给网络搜索 Agent 的问题描述"
                }
            },
            "required": ["question"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let AgentWebSearchArgs { question } = parse_arguments(self.name(), arguments)?;
        let question = question.trim();
        if question.is_empty() {
            anyhow::bail!("联网搜索内容不能为空");
        }

        let max_attempts = context.services.app_config.app.ai_request_max_attempts();
        let timeout_seconds = context.services.app_config.app.ai_request_timeout_seconds;
        let search_prompt = render_prompt_template(
            &context
                .services
                .app_config
                .prompt_config
                .web_search_agent_prompt,
            &[("question", question)],
        );
        let mut last_error = None;
        for attempt in 1..=max_attempts {
            let provider = context
                .services
                .app_config
                .ai_models
                .get(&context.services.app_config.app.web_search_model_name)
                .ok_or_else(|| anyhow::anyhow!("找不到联网搜索模型配置"))?;
            let search_result = if timeout_seconds == 0 {
                provider.web_search(&search_prompt).await
            } else {
                match timeout(
                    Duration::from_secs(timeout_seconds),
                    provider.web_search(&search_prompt),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(anyhow::anyhow!(
                        "联网搜索 API 请求超时，超过 {} 秒",
                        timeout_seconds
                    )),
                }
            };

            match search_result {
                Ok(content) => return Ok(ToolOutput::text(content)),
                Err(error) => {
                    warn!(attempt, max_attempts, error = %error, "联网搜索 API 请求失败");
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.expect("联网搜索重试循环至少应执行一次"))
    }
}
