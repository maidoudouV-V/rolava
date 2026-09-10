use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"设置群聊后续关注状态，两种状态都会结束本轮处理，可以同时输出聊天正文。
state 为 continue 时，后续群消息继续直接交给你处理；为 end 时，后续群消息先经过筛选，只有需要你处理时才交给你。
应在必要操作完成后调用，不与其他工具一起调用。"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ConversationState {
    Continue,
    End,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetConversationStateArgs {
    state: ConversationState,
}

pub struct SetConversationStateTool;

#[async_trait]
impl Tool for SetConversationStateTool {
    fn name(&self) -> &'static str {
        "set_conversation_state"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "state": {
                    "type": "string",
                    "enum": ["continue", "end"],
                    "description": "continue：后续群消息继续直接交给你处理；end：后续群消息先经过筛选，只有需要你处理时才交给你"
                }
            },
            "required": ["state"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let arguments: SetConversationStateArgs = parse_arguments(self.name(), arguments)?;
        match arguments.state {
            ConversationState::Continue => {
                context.conversation.control.set_ai_filter_bypassed(true);
                Ok(ToolOutput::continue_conversation())
            }
            ConversationState::End => {
                context.conversation.control.set_ai_filter_bypassed(false);
                Ok(ToolOutput::end_conversation())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_is_explicit_and_invalid_arguments_are_rejected() {
        for arguments in [
            r#"{}"#,
            r#"{"state":"other"}"#,
            r#"{"state":true}"#,
            r#"{"state":"end","extra":1}"#,
        ] {
            assert!(parse_arguments::<SetConversationStateArgs>(
                "set_conversation_state",
                arguments
            )
            .is_err());
        }
        for (arguments, expected_continue) in [
            (r#"{"state":"continue"}"#, true),
            (r#"{"state":"end"}"#, false),
        ] {
            let parsed: SetConversationStateArgs =
                parse_arguments("set_conversation_state", arguments).unwrap();
            assert_eq!(
                matches!(parsed.state, ConversationState::Continue),
                expected_continue
            );
        }
    }
}
