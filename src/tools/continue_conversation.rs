use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"当本轮无需发送聊天正文，但仍需关注后续对话时，调用此工具结束本轮处理。
在群聊中，调用后会继续将后续消息交给你处理；在私聊中，仅结束本轮处理。
应在必要操作完成后单独调用，不同时输出正文，也不与其他工具一起调用。"#;

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
