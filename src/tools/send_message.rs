use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};
use crate::transport::SendOptions;

const DESCRIPTION: &str = r#"向当前聊天窗口发送一条消息。
当角色需要回复当前会话时调用。每次调用只发送一条消息；需要连续发送多条时可以多次调用。
你的回复应该符合聊天软件风格，你的发言要像真人即时打字：自然、口语化、通常 1 句话或 1～2 个短句，不要长篇解释。
消息中不要包含说话人前缀、心理活动、动作描写或括号旁白。"#;

#[derive(Debug, Deserialize)]
pub struct SendMessageArgs {
    pub text: String,
}

pub struct SendMessageTool;

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &'static str {
        "send_message"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "发送给当前聊天窗口的完整文本"
                }
            },
            "required": ["text"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let SendMessageArgs { text } = parse_arguments(self.name(), arguments)?;
        context
            .services
            .message_sender
            .send_text(&context.conversation.target, &text, SendOptions::default())
            .await?;

        Ok(ToolOutput::text("消息已发送"))
    }
}
