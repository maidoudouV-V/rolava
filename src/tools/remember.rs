use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{execution_not_implemented, parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"记录一条需要长期保存的信息。

只记录明确、有价值且未来可能用到的信息，不要记录临时闲聊、情绪宣泄或不确定的猜测。"#;

#[derive(Debug, Deserialize)]
pub struct RememberArgs {
    pub content: String,
}

pub struct RememberTool;

#[async_trait]
impl Tool for RememberTool {
    fn name(&self) -> &'static str {
        "remember"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "需要长期保存的完整信息"
                }
            },
            "required": ["content"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, _context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let _arguments: RememberArgs = parse_arguments(self.name(), arguments)?;

        /* 旧动作目前没有真实存储实现，仅暂存原有行为。
        let RememberArgs { content } = _arguments;
        tracing::warn!(content = %content, "暂未实现记忆写入");
        return Ok(ToolOutput::text("记忆写入尚未实现"));
        */

        execution_not_implemented(self.name())
    }
}
