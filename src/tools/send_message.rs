use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};
use crate::transport::SendOptions;

const DESCRIPTION: &str = r#"向当前聊天窗口发送一条消息。未使用，仅占位"#;

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
