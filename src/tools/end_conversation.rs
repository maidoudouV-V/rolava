use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"结束查看实时聊天内容。
降低查看群消息频率，不再实时查看每条聊天内容，只在群聊场景中生效。
在当前聊天内容与你无关时调用。"#;

pub struct EndConversationTool;

#[async_trait]
impl Tool for EndConversationTool {
    fn name(&self) -> &'static str {
        "end_conversation"
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
        context.conversation.control.set_ai_filter_bypassed(false);
        Ok(ToolOutput::end_conversation())
    }
}
