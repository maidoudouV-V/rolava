use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"删除一条关于指定群友或好友的长期记忆。
user_id 必须使用最近活跃用户中显示的真实 QQ 号，memory_id 必须原样使用对方记忆中显示的稳定 ID。
仅当记忆已经失效且不再需要保留时删除；需要纠正或补充内容时，应使用 set_user_memory 并传入 memory_id 更新。"#;

#[derive(Debug, Deserialize)]
pub struct DeleteUserMemoryArgs {
    pub user_id: String,
    pub memory_id: String,
}

pub struct DeleteUserMemoryTool;

#[async_trait]
impl Tool for DeleteUserMemoryTool {
    fn name(&self) -> &'static str {
        "delete_user_memory"
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
                    "description": "对方已有记忆中需要删除的稳定 ID",
                    "minLength": 1
                }
            },
            "required": ["user_id", "memory_id"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let arguments: DeleteUserMemoryArgs = parse_arguments(self.name(), arguments)?;
        let result = context
            .conversation
            .user_memory
            .delete_memory(&arguments.user_id, &arguments.memory_id)?;
        Ok(ToolOutput::text(result))
    }
}
