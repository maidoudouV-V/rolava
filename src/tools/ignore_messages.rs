use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = "忽略触发本轮处理的当前消息，不发送回复，也不修改任何会话状态。";

pub struct IgnoreMessagesTool;

#[async_trait]
impl Tool for IgnoreMessagesTool {
    fn name(&self) -> &'static str {
        "ignore_messages"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, _context: &ToolContext, _arguments: &str) -> Result<ToolOutput> {
        Ok(ToolOutput {
            content: "已忽略当前消息".to_string(),
        })
    }
}
