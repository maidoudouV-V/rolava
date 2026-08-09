use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"删除当前会话的一条角色记忆。
title 必须与提示词中显示的角色记忆标题完全一致，不要猜测或模糊匹配。
仅当记忆已经失效且不再需要保留时调用此工具删除；需要修改内容或续期时，应使用 set_character_memory。"#;

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
