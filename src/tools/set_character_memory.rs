use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"为当前会话新增或更新一条扮演角色的长期记忆。
title 是记忆的标题，新增时必须提供新的标题，修改时必须与已有标题完全一致。
同时新增时必须提供 content 和 retention_days；修改时只传需要更新的字段。传入 retention_days 会从当前时间重新计算保留期限。
角色记忆用于保存会影响你后续行动的重要共同经历、明确约定、待完成计划或有持续影响的关系变化；出现这类信息时应主动记录，无需等待用户要求。对于成员自身的资料应使用 create_user_memory 或 update_user_memory。
不必每轮都记录。普通寒暄、一次玩笑、短暂情绪和没有后续影响的日常琐事通常不保存；不要把每次互动都解释成关系进展。
同一主题已有记忆时优先更新，仅在事实或后续安排发生实质变化时修改，避免重复记录或仅为润色而改写。
记忆内容涉及具体用户时，必须使用“昵称（QQ号）”标识每位相关用户，不能只写昵称。QQ号必须来自当前上下文中已确认的身份信息，不得猜测或编造。
retention_days 应与事情预计仍有用的时间相符，不默认使用最长时限。记忆显示“即将遗忘”时，如后续时间还会用到此记忆，重新更新 retention_days 续期记忆。
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
