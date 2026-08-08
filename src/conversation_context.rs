use serde::{Deserialize, Serialize};

use crate::ai_provider::ToolChatMessage;

/// 一次模型响应及其随后产生的全部工具结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiTurnRound {
    /// 模型返回的 assistant 消息，只持久化正文和工具调用，不保存推理过程。
    pub assistant: ToolChatMessage,
    /// 本轮全部工具结果，顺序与 assistant 中的工具调用保持一致。
    pub tool_results: Vec<ToolChatMessage>,
}

impl AiTurnRound {
    /// 构造长期上下文记录，移除只应在当前工具循环内回传的推理过程。
    pub fn for_history(assistant: ToolChatMessage, tool_results: Vec<ToolChatMessage>) -> Self {
        let assistant = match assistant {
            ToolChatMessage::Assistant {
                content,
                tool_calls,
                ..
            } => ToolChatMessage::Assistant {
                content,
                reasoning: None,
                tool_calls,
            },
            other => other,
        };

        Self {
            assistant,
            tool_results,
        }
    }
}

/// 从一次会话触发开始，到工具循环结束为止的完整模型上下文原子块。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AiTurnBlock {
    /// 按实际发生顺序保存的全部模型响应和工具结果。
    pub rounds: Vec<AiTurnRound>,
}

impl AiTurnBlock {
    /// 按 Provider 请求协议要求展开完整消息序列。
    pub fn extend_messages(&self, messages: &mut Vec<ToolChatMessage>) {
        for round in &self.rounds {
            messages.push(round.assistant.clone());
            messages.extend(round.tool_results.iter().cloned());
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{AiTurnBlock, AiTurnRound};
    use crate::ai_provider::{ReasoningPayload, ToolChatMessage};
    use crate::tools::ToolCall;

    #[test]
    fn ai_turn_round_trips_tool_protocol_without_reasoning() {
        let block = AiTurnBlock {
            rounds: vec![AiTurnRound::for_history(
                ToolChatMessage::Assistant {
                    content: Some("我查一下".to_string()),
                    reasoning: Some(ReasoningPayload::Structured(json!([{
                        "type": "reasoning.text",
                        "text": "检查信息"
                    }]))),
                    tool_calls: vec![ToolCall {
                        id: "call-1".to_string(),
                        name: "agent_web_search".to_string(),
                        arguments: r#"{"question":"测试"}"#.to_string(),
                    }],
                },
                vec![ToolChatMessage::Tool {
                    tool_call_id: "call-1".to_string(),
                    content: "搜索结果".to_string(),
                }],
            )],
        };

        let payload = serde_json::to_string(&block).unwrap();
        assert!(!payload.contains("reasoning"));
        assert!(!payload.contains("检查信息"));
        let restored: AiTurnBlock = serde_json::from_str(&payload).unwrap();
        let mut messages = Vec::new();
        restored.extend_messages(&mut messages);

        assert_eq!(messages.len(), 2);
        match &messages[0] {
            ToolChatMessage::Assistant {
                content,
                reasoning,
                tool_calls,
            } => {
                assert_eq!(content.as_deref(), Some("我查一下"));
                assert!(reasoning.is_none());
                assert_eq!(tool_calls[0].id, "call-1");
                assert_eq!(tool_calls[0].name, "agent_web_search");
            }
            other => panic!("第一条应为 assistant，实际为 {other:?}"),
        }
        match &messages[1] {
            ToolChatMessage::Tool {
                tool_call_id,
                content,
            } => {
                assert_eq!(tool_call_id, "call-1");
                assert_eq!(content, "搜索结果");
            }
            other => panic!("第二条应为 tool，实际为 {other:?}"),
        }
    }
}
