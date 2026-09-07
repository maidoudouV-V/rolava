use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"删除一条你在当前会话中不再需要保留的群记忆。
仅当记忆已经失效且没有后续价值，或确认与另一条保留的记忆重复时删除。
事情完成但仍有保留价值时，不必删除。
需要纠正内容、补充进展或续期时，使用 set_group_memory，不要先删除再重建。
title 必须原样使用当前群记忆中显示的标题，不要猜测或模糊匹配。"#;

#[derive(Debug, Deserialize)]
pub struct DeleteGroupMemoryArgs {
    pub title: String,
}

pub struct DeleteGroupMemoryTool;

#[async_trait]
impl Tool for DeleteGroupMemoryTool {
    fn name(&self) -> &'static str {
        "delete_group_memory"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "需要删除的群记忆标题",
                    "minLength": 1,
                    "maxLength": 50
                }
            },
            "required": ["title"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let arguments: DeleteGroupMemoryArgs = parse_arguments(self.name(), arguments)?;
        let result = context
            .conversation
            .group_memory
            .delete_memory(&arguments.title)?;
        Ok(ToolOutput::text(result))
    }
}
