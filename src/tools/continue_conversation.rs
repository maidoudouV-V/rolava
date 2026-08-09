use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"继续查看后续对话。
只在群聊场景中生效。
当选择本轮对话不进行回复，但还要继续查看后续聊天内容时调用。"#;

pub struct ContinueConversationTool;

#[async_trait]
impl Tool for ContinueConversationTool {
    fn name(&self) -> &'static str {
        "continue_conversation"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, _arguments: &str) -> Result<ToolOutput> {
        context.conversation.control.set_ai_filter_bypassed(true);
        Ok(ToolOutput::continue_conversation())
    }
}
