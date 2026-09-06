use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"新增一条关于指定群友或好友的长期记忆。
当聊天内容中明确透露较稳定、能帮助你以后理解或回应对方的信息时，应主动记录，无需等待对方要求“记住”；例如长期偏好、惯用称呼、身份背景、真实姓名或重要关系。
在持续互动中，如果你对对方形成了较为稳定的印象，可以记录你对对方的主观感受和关系认知；不要记录一时情绪，也不要仅因对话次数增加就推断关系加深。
不必每轮都记录。一次性的吃喝行程、随口感叹、玩笑和未经确认的推测通常不值得保存；不要把一次行为概括成长期习惯或偏好。
user_id 必须使用最近活跃用户中显示的真实 QQ 号。
记忆内容涉及具体群友或好友时，必须使用“昵称（QQ号）”标识每位相关的人，不能只写昵称。QQ号必须来自当前上下文中已确认的身份信息，不得猜测或编造。
仅当对方现有记忆中没有相同主题时新增；已有相关记忆时应调用 update_user_memory，避免重复。
content 应简洁、完整且可以独立理解。客观信息与你的主观感受应明确区分，只保留有后续价值的内容，不写聊天流水账。"#;

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
