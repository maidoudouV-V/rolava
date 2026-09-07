use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"新增一条关于指定群友或好友的长期记忆。
user_id 必须使用最近活跃用户中显示的真实 QQ 号。
仅当对方现有记忆中没有相同主题时新增；已有相关记忆时应调用 update_user_memory，避免重复。
content 是需要保存的完整记忆内容。"#;

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
                    "description": "需要长期保留的关于对方的完整记忆",
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
