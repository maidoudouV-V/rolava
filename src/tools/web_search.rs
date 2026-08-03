use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{execution_not_implemented, parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"检索互联网中的实时或近期信息，并把查询结果返回给当前工具调用。

适用于新闻、公告、版本变化、价格、比赛结果或指定网页内容。查询应包含完成检索所需的必要上下文。"#;

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

    async fn execute(&self, _context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let _arguments: WebSearchArgs = parse_arguments(self.name(), arguments)?;

        /* 旧动作实现暂存，等待统一的请求重试器注入 ToolContext 后再接入。
        let WebSearchArgs { query } = _arguments;
        let max_attempts = context.app_config.app.ai_request_max_attempts();
        let mut last_error = None;
        for attempt in 1..=max_attempts {
            let provider = context
                .app_config
                .ai_models
                .get(&context.app_config.app.web_search_model_name)
                .ok_or_else(|| anyhow::anyhow!("找不到联网搜索模型配置"))?;
            match provider.web_search(&query).await {
                Ok(content) => return Ok(ToolOutput { content }),
                Err(error) => {
                    eprintln!(
                        "联网搜索 API 请求失败，第 {}/{} 次: {}",
                        attempt, max_attempts, error
                    );
                    last_error = Some(error);
                }
            }
        }
        return Err(last_error.expect("联网搜索重试循环至少应执行一次"));
        */

        execution_not_implemented(self.name())
    }
}
