use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"更新对指定群友/好友的长期记忆。
当聊天内容中发现某个群友已记录的信息发生变化、补充或你推断出已有记忆出错时，应主动更新对应记忆，无需等待用户要求。
重复提及、换一种说法或短暂状态变化不需要更新；不要为了润色措辞而反复改写，也不要用未经确认的推测覆盖已有事实。
user_id 必须使用最近活跃用户中显示的真实 QQ 号，memory_id 必须原样使用该用户记忆中显示的 `mem_...` 稳定 ID。
记忆内容涉及具体用户时，必须使用“昵称（QQ号）”标识每位相关用户，不能只写昵称。QQ号必须来自当前上下文中已确认的身份信息，不得猜测或编造。
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
                    "description": "已有用户记忆中显示的稳定 ID",
                    "pattern": "^mem_[A-Za-z0-9]{8}$"
                },
                "content": {
                    "type": "string",
                    "description": "更新后的完整用户记忆",
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
