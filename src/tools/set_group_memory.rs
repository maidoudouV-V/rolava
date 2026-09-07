use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"新增或更新一条你在当前会话中需要长期保留的群记忆。
title 是当前会话内的唯一标题。新增前先检查是否已有相关记忆；修改时必须原样使用已有标题。
新增时必须提供 content 和 retention_days；修改时至少提供其中一项。content 是替换后的完整内容，应保留仍然有效的信息。
retention_days 为 0 表示长期（永不过期），1～365 表示从当前时间起保留的天数；修改时省略则保持原期限。可在长期与限期之间切换，仅修改期限无需传入 content。
"#;

#[derive(Debug, Deserialize)]
pub struct SetGroupMemoryArgs {
    pub title: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub retention_days: Option<u16>,
}

pub struct SetGroupMemoryTool;

#[async_trait]
impl Tool for SetGroupMemoryTool {
    fn name(&self) -> &'static str {
        "set_group_memory"
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
                    "description": "记忆标题，也是修改记忆的唯一键",
                    "minLength": 1,
                    "maxLength": 50
                },
                "content": {
                    "type": "string",
                    "description": "完整记忆内容；新建时必填，修改时可省略",
                    "minLength": 1,
                    "maxLength": 1000
                },
                "retention_days": {
                    "type": "integer",
                    "description": "0 表示长期（永不过期）；1～365 表示从现在起保留的天数；新建必填，修改时省略则保留原期限",
                    "minimum": 0,
                    "maximum": 365
                }
            },
            "required": ["title"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let arguments: SetGroupMemoryArgs = parse_arguments(self.name(), arguments)?;
        let result = context.conversation.group_memory.set_memory(
            &arguments.title,
            arguments.content.as_deref(),
            arguments.retention_days,
        )?;
        Ok(ToolOutput::text(result))
    }
}
