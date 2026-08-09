use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"为当前会话新增或更新一条角色长期记忆。
title 是当前会话内的稳定唯一键，修改时必须与已有标题完全一致。
新建时必须提供 content 和 retention_days；修改时只传需要更新的字段。传入 retention_days 会从当前时间重新计算保留期限。
角色记忆用于保存你扮演的角色在当前会话中需要跨轮次保留的经历、约定、计划、关系进展或持续状态；对于成员自身的资料应使用 create_user_memory 或 update_user_memory。
记忆显示“即将遗忘”时，可仅设置 retention_days 进行续期。
当需要长时间记住某些事时调用此工具。
当某些记忆发生变化时，调用此工具及时更新记忆，以保持内容准确。
"#;

#[derive(Debug, Deserialize)]
pub struct SetCharacterMemoryArgs {
    pub title: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub retention_days: Option<u16>,
}

pub struct SetCharacterMemoryTool;

#[async_trait]
impl Tool for SetCharacterMemoryTool {
    fn name(&self) -> &'static str {
        "set_character_memory"
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
                    "description": "记忆的会话内唯一标题",
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
                    "description": "从现在开始记忆保留的天数；新建或续期时填写",
                    "minimum": 1,
                    "maximum": 365
                }
            },
            "required": ["title"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let arguments: SetCharacterMemoryArgs = parse_arguments(self.name(), arguments)?;
        let result = context.conversation.character_memory.set_memory(
            &arguments.title,
            arguments.content.as_deref(),
            arguments.retention_days,
        )?;
        Ok(ToolOutput::text(result))
    }
}
