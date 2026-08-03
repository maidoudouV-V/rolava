use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::{timeout, Duration};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"通过在线AI模型检索互联网中的实时或近期信息。
查询应包含完成检索所需的必要上下文。
适用于新闻、公告、版本变化、价格、比赛结果或指定网页内容。"#;

#[derive(Debug, Deserialize)]
pub struct WebSearchArgs {
    pub query: String,
}

pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "具体、完整的自然语言查询，可包含需要读取的 URL"
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let WebSearchArgs { query } = parse_arguments(self.name(), arguments)?;
        let query = query.trim();
        if query.is_empty() {
            anyhow::bail!("联网搜索内容不能为空");
        }

        let max_attempts = context.services.app_config.app.ai_request_max_attempts();
        let timeout_seconds = context.services.app_config.app.ai_request_timeout_seconds;
        let mut last_error = None;
        for attempt in 1..=max_attempts {
            let provider = context
                .services
                .app_config
                .ai_models
                .get(&context.services.app_config.app.web_search_model_name)
                .ok_or_else(|| anyhow::anyhow!("找不到联网搜索模型配置"))?;
            let search_result = if timeout_seconds == 0 {
                provider.web_search(query).await
            } else {
                match timeout(
                    Duration::from_secs(timeout_seconds),
                    provider.web_search(query),
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
                    eprintln!(
                        "联网搜索 API 请求失败，第 {}/{} 次: {}",
                        attempt, max_attempts, error
                    );
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.expect("联网搜索重试循环至少应执行一次"))
    }
}
