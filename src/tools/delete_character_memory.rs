use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"删除一条你在当前会话中不再需要保留的角色记忆。
仅当记忆已经失效且没有后续价值，或确认与另一条保留的记忆重复时删除。
事情已经完成不代表必须删除；如果这段经历仍有值得保留的意义，可以继续保留。
需要纠正内容、补充进展或续期时，使用 set_character_memory，不要先删除再重建。
title 必须原样使用当前角色记忆中显示的标题，不要猜测或模糊匹配。
根据记忆所在的位置选择工具：当前角色记忆中的条目使用本工具；最近活跃用户中列出的个人记忆使用 delete_user_memory。"#;

#[derive(Debug, Deserialize)]
pub struct DeleteCharacterMemoryArgs {
    pub title: String,
}

pub struct DeleteCharacterMemoryTool;

#[async_trait]
impl Tool for DeleteCharacterMemoryTool {
    fn name(&self) -> &'static str {
        "delete_character_memory"
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
                    "description": "需要删除的角色记忆标题",
                    "minLength": 1,
                    "maxLength": 50
                }
            },
            "required": ["title"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let arguments: DeleteCharacterMemoryArgs = parse_arguments(self.name(), arguments)?;
        let result = context
            .conversation
            .character_memory
            .delete_memory(&arguments.title)?;
        Ok(ToolOutput::text(result))
    }
}
