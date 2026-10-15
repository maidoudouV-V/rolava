use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"仅在决定不回复任何正文、且后续无需继续调用其他工具时，调用此工具静默结束当前工具循环。
调用时不输出正文；调用后不再请求模型继续响应。
需要回复正文时，直接输出正文结束本轮响应，无需调用此工具。"#;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinishResponseArgs {}

pub struct FinishResponseTool;

#[async_trait]
impl Tool for FinishResponseTool {
    fn name(&self) -> &'static str {
        "finish_response"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        })
    }

    async fn execute(&self, _context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let _: FinishResponseArgs = parse_arguments(self.name(), arguments)?;
        Ok(ToolOutput::new("", false))
    }
}
