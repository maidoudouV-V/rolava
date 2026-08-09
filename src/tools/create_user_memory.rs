use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"为最近活跃用户新增一条跨轮次用户记忆。
user_id 必须使用最近活跃用户中显示的真实 QQ 号。
仅当该用户现有记忆中没有相同主题时新增；已有相关记忆时应调用 update_user_memory，避免重复。
content 应简洁、完整且可以独立理解。仅记录较稳定、以后仍有价值的信息，例如称呼、身份、偏好、习惯、关系和已经确认的重要事实；不要记录临时状态、普通闲聊或未经确认的推测。"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateUserMemoryArgs {
    pub user_id: String,
    pub content: String,
}

pub struct CreateUserMemoryTool;

#[async_trait]
impl Tool for CreateUserMemoryTool {
    fn name(&self) -> &'static str {
        "create_user_memory"
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
                "content": {
                    "type": "string",
                    "description": "需要长期保留的完整用户记忆",
                    "minLength": 1
                }
            },
            "required": ["user_id", "content"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let arguments: CreateUserMemoryArgs = parse_arguments(self.name(), arguments)?;
        let result = context
            .conversation
            .user_memory
            .create_memory(&arguments.user_id, &arguments.content)?;
        Ok(ToolOutput::text(result))
    }
}
