use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"更新一条关于指定群友或好友的长期记忆。
user_id 必须使用最近活跃用户中显示的真实 QQ 号，memory_id 必须原样使用对方记忆中显示的 `mem_...` 稳定 ID。
content 是更新后的完整记忆内容，不是只包含本次变化的补丁；保留仍然有效的信息，删去被明确纠正的旧内容。"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateUserMemoryArgs {
    pub user_id: String,
    pub memory_id: String,
    pub content: String,
}

pub struct UpdateUserMemoryTool;

#[async_trait]
impl Tool for UpdateUserMemoryTool {
    fn name(&self) -> &'static str {
        "update_user_memory"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "user_id": {
                    "type": "string",
                    "description": "最近活跃用户中显示的真实 QQ 号",
                    "minLength": 1
                },
                "memory_id": {
                    "type": "string",
                    "description": "对方已有记忆中显示的稳定 ID",
                    "pattern": "^mem_[A-Za-z0-9]{8}$"
                },
                "content": {
                    "type": "string",
                    "description": "更新后的关于对方的完整记忆",
                    "minLength": 1
                }
            },
            "required": ["user_id", "memory_id", "content"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let arguments: UpdateUserMemoryArgs = parse_arguments(self.name(), arguments)?;
        let result = context.conversation.user_memory.update_memory(
            &arguments.user_id,
            &arguments.memory_id,
            &arguments.content,
        )?;
        Ok(ToolOutput::text(result))
    }
}
