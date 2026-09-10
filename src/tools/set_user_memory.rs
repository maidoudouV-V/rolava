use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"新增或更新一条关于指定群友或好友的长期记忆。
user_id 必须使用最近活跃用户中显示的真实 QQ 号。
省略 memory_id 时新增；已有相同主题的记忆时，必须传入对方记忆中显示的 `mem_...` 稳定 ID 进行更新，避免重复。指定 ID 不存在时会报错，不会新增。
content 是保存后的完整记忆内容，不是补丁；更新时保留仍然有效的信息，删去被明确纠正的旧内容。"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetUserMemoryArgs {
    pub user_id: String,
    pub memory_id: Option<String>,
    pub content: String,
}

pub struct SetUserMemoryTool;

#[async_trait]
impl Tool for SetUserMemoryTool {
    fn name(&self) -> &'static str {
        "set_user_memory"
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
                    "description": "更新时必须传入对方已有记忆的稳定 ID；新增时省略",
                    "pattern": "^mem_[A-Za-z0-9]{8}$"
                },
                "content": {
                    "type": "string",
                    "description": "需要长期保留的关于对方的完整记忆",
                    "minLength": 1,
                    "maxLength": crate::memory::MAX_USER_MEMORY_CONTENT_CHARS
                }
            },
            "required": ["user_id", "content"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let arguments: SetUserMemoryArgs = parse_arguments(self.name(), arguments)?;
        let memory = &context.conversation.user_memory;
        let result = match arguments.memory_id.as_deref() {
            Some(memory_id) => {
                memory.update_memory(&arguments.user_id, memory_id, &arguments.content)?
            }
            None => memory.create_memory(&arguments.user_id, &arguments.content)?,
        };
        Ok(ToolOutput::text(result))
    }
}
