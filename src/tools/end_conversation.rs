use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"当不再需要关注当前话题时，调用此工具结束本轮处理，可以同时输出聊天正文。
在群聊中，调用后恢复常规消息筛选，不再持续将每条消息交给你处理；在私聊中，仅结束本轮处理。
应在必要操作完成后调用，不与其他工具一起调用。"#;

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
