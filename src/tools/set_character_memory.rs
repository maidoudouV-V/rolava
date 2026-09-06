use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"新增或更新一条你在当前会话中需要长期保留的角色记忆。
用于保存你自己的重要经历、明确承诺、待完成计划，以及会持续影响后续行动的事情。出现值得保留的内容时，可以主动记录，无需等待对方要求。
关于某位群友或好友的资料、偏好，以及你对对方的稳定印象，应使用 create_user_memory 或 update_user_memory；涉及你自己的经历、约定或计划，即使与某个人有关，也可以保存在这里，避免两边重复记录。
不必每轮都记录。普通寒暄、玩笑、短暂情绪和没有后续影响的日常琐事通常不保存。
同一件事已有记忆时优先更新；只有内容或进展发生实质变化时才修改，不为润色措辞反复改写。
title 是当前会话内的唯一标题。新增前先检查是否已有相关记忆；修改时必须原样使用已有标题。
新增时必须提供 content 和 retention_days；修改时只传需要更新的字段。content 应简洁、完整，保留仍然有效的信息。
涉及具体群友或好友时，使用“昵称（QQ号）”标识，QQ号必须来自已确认的上下文，不得猜测或编造。
retention_days 按事情预计仍有用的时间设置，不默认使用最长时限；传入后会从当前时间重新计算期限。
记忆即将遗忘时，只有事情尚未完成或仍有明确的后续价值，才续期；仅续期时无需重写内容。
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
                    "description": "从现在开始记忆保留的天数；新建或续期时填写；根据实际情况填写合理保留天数",
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
