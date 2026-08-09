use std::collections::HashSet;

use anyhow::Result;

use crate::ai_provider::ToolChatMessage;

/// 一个只在当前进程内保留的完整工具调用轮次。
#[derive(Debug, Clone)]
pub struct ToolRoundHistory {
    assistant: ToolChatMessage,
    tool_results: Vec<ToolChatMessage>,
}

impl ToolRoundHistory {
    /// 跨请求只保留工具协议，模型推理仍只存在于当前工具循环。
    pub fn new(assistant: ToolChatMessage, tool_results: Vec<ToolChatMessage>) -> Result<Self> {
        let assistant = match assistant {
            ToolChatMessage::Assistant {
                content,
                tool_calls,
                ..
            } if !tool_calls.is_empty() => ToolChatMessage::Assistant {
                content,
                reasoning: None,
                tool_calls,
            },
            _ => anyhow::bail!("内存工具轮次缺少 assistant tool_calls"),
        };
        let history = Self {
            assistant,
            tool_results,
        };
        history.validate()?;
        Ok(history)
    }

    fn extend_messages(&self, messages: &mut Vec<ToolChatMessage>) {
        messages.push(self.assistant.clone());
        messages.extend(self.tool_results.iter().cloned());
    }

    fn validate(&self) -> Result<()> {
        let ToolChatMessage::Assistant { tool_calls, .. } = &self.assistant else {
            anyhow::bail!("内存工具轮次的首条消息不是 assistant");
        };
        let expected_ids = tool_calls
            .iter()
            .map(|call| call.id.as_str())
            .collect::<HashSet<_>>();
        if expected_ids.len() != tool_calls.len() {
            anyhow::bail!("内存工具轮次包含重复的 tool_call ID");
        }

        let mut actual_ids = HashSet::new();
        for result in &self.tool_results {
            let ToolChatMessage::Tool { tool_call_id, .. } = result else {
                anyhow::bail!("内存工具轮次包含非 tool 结果");
            };
            if !actual_ids.insert(tool_call_id.as_str()) {
                anyhow::bail!("内存工具轮次包含重复的 tool 结果");
            }
        }
        if expected_ids != actual_ids {
            anyhow::bail!("内存工具轮次的调用和结果无法一一对应");
        }
        Ok(())
    }
}

/// 一次主处理产生的全部工具轮次及其在可见聊天记录中的位置。
#[derive(Debug, Clone)]
pub struct ActiveToolHistory {
    pub after_message_id: Option<i64>,
    rounds: Vec<ToolRoundHistory>,
    suppressed_message_ids: HashSet<i64>,
}

impl ActiveToolHistory {
    pub fn new(
        after_message_id: Option<i64>,
        rounds: Vec<ToolRoundHistory>,
        suppressed_message_ids: impl IntoIterator<Item = i64>,
    ) -> Result<Self> {
        if rounds.is_empty() {
            anyhow::bail!("不能保存空的工具历史");
        }
        let history = Self {
            after_message_id,
            rounds,
            suppressed_message_ids: suppressed_message_ids.into_iter().collect(),
        };
        history.validate()?;
        Ok(history)
    }

    pub fn suppresses_message(&self, message_id: i64) -> bool {
        self.suppressed_message_ids.contains(&message_id)
    }

    pub fn extend_messages(&self, messages: &mut Vec<ToolChatMessage>) {
        for round in &self.rounds {
            round.extend_messages(messages);
        }
    }

    fn retain_visible_messages(&mut self, visible_ids: &HashSet<i64>) {
        self.suppressed_message_ids
            .retain(|message_id| visible_ids.contains(message_id));
    }

    fn validate(&self) -> Result<()> {
        for round in &self.rounds {
            round.validate()?;
        }
        Ok(())
    }
}

/// 单个 ConversationActor 在本次进程生命周期内维护的上下文布局。
#[derive(Debug, Default)]
pub struct RuntimeContextState {
    loaded_message_ids: Vec<i64>,
    sealed_after_message_ids: HashSet<i64>,
    active_tool_histories: Vec<ActiveToolHistory>,
}

impl RuntimeContextState {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// 同步数据库窗口；只有窗口淘汰或删除记录时才放弃原有分块边界。
    pub fn reconcile_message_ids(&mut self, message_ids: &[i64]) -> Result<bool> {
        if message_ids.windows(2).any(|ids| ids[0] >= ids[1]) {
            anyhow::bail!("数据库聊天窗口的消息 ID 不是严格递增顺序");
        }

        let rebuilt = !self.loaded_message_ids.is_empty()
            && !message_ids.starts_with(&self.loaded_message_ids);
        let visible_ids = message_ids.iter().copied().collect::<HashSet<_>>();
        if rebuilt {
            self.sealed_after_message_ids.clear();
            self.active_tool_histories.retain(|history| {
                history
                    .after_message_id
                    .is_some_and(|message_id| visible_ids.contains(&message_id))
            });
        }

        for history in &mut self.active_tool_histories {
            history.retain_visible_messages(&visible_ids);
        }
        self.loaded_message_ids.clear();
        self.loaded_message_ids.extend_from_slice(message_ids);
        self.validate()?;
        Ok(rebuilt)
    }

    pub fn seal_current_tail(&mut self) {
        if let Some(message_id) = self.loaded_message_ids.last().copied() {
            self.sealed_after_message_ids.insert(message_id);
        }
    }

    pub fn is_sealed_after(&self, message_id: i64) -> bool {
        self.sealed_after_message_ids.contains(&message_id)
    }

    pub fn active_tool_histories(&self) -> &[ActiveToolHistory] {
        &self.active_tool_histories
    }

    pub fn push_tool_history(&mut self, history: ActiveToolHistory) -> Result<()> {
        self.active_tool_histories.push(history);
        self.validate()
    }

    /// 对话结束后只保留 messages 中已经真实发送的正文，不再保留工具协议。
    pub fn compact_finished_conversation(&mut self) -> usize {
        let removed = self.active_tool_histories.len();
        self.active_tool_histories.clear();
        removed
    }

    fn validate(&self) -> Result<()> {
        let loaded_ids = self
            .loaded_message_ids
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        if self
            .sealed_after_message_ids
            .iter()
            .any(|message_id| !loaded_ids.contains(message_id))
        {
            anyhow::bail!("内存上下文包含已经离开窗口的分块边界");
        }

        let mut suppressed_ids = HashSet::new();
        for history in &self.active_tool_histories {
            history.validate()?;
            if let Some(anchor) = history.after_message_id {
                if !loaded_ids.contains(&anchor) {
                    anyhow::bail!("内存工具历史的上下文锚点已经离开窗口");
                }
            }
            for message_id in &history.suppressed_message_ids {
                if !suppressed_ids.insert(*message_id) {
                    anyhow::bail!("同一条可见消息被多个工具历史重复占用");
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{ActiveToolHistory, RuntimeContextState, ToolRoundHistory};
    use crate::ai_provider::ToolChatMessage;
    use crate::tools::ToolCall;

    #[test]
    fn stable_append_keeps_boundaries_and_window_rebuild_discards_them() {
        let mut state = RuntimeContextState::default();
        assert!(!state.reconcile_message_ids(&[1, 2]).unwrap());
        state.seal_current_tail();
        assert!(state.is_sealed_after(2));

        assert!(!state.reconcile_message_ids(&[1, 2, 3]).unwrap());
        assert!(state.is_sealed_after(2));

        assert!(state.reconcile_message_ids(&[3, 4]).unwrap());
        assert!(!state.is_sealed_after(2));
    }

    #[test]
    fn invalid_database_order_is_rejected() {
        let mut state = RuntimeContextState::default();
        assert!(state.reconcile_message_ids(&[2, 1]).is_err());
        assert!(state.reconcile_message_ids(&[1, 1]).is_err());
    }

    #[test]
    fn tool_history_is_validated_reconciled_and_compacted() {
        let round = ToolRoundHistory::new(
            ToolChatMessage::Assistant {
                content: Some("正在查询".to_string()),
                reasoning: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "test_tool".to_string(),
                    arguments: "{}".to_string(),
                }],
            },
            vec![ToolChatMessage::Tool {
                tool_call_id: "call-1".to_string(),
                content: "查询结果".to_string(),
            }],
        )
        .unwrap();
        let history = ActiveToolHistory::new(Some(2), vec![round], [3]).unwrap();
        let mut state = RuntimeContextState::default();
        state.reconcile_message_ids(&[1, 2, 3]).unwrap();
        state.push_tool_history(history).unwrap();

        assert!(state.reconcile_message_ids(&[2, 3, 4]).unwrap());
        assert_eq!(state.active_tool_histories().len(), 1);
        assert_eq!(state.compact_finished_conversation(), 1);
        assert!(state.active_tool_histories().is_empty());
    }

    #[test]
    fn mismatched_tool_result_is_rejected() {
        let result = ToolRoundHistory::new(
            ToolChatMessage::Assistant {
                content: None,
                reasoning: None,
                tool_calls: vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "test_tool".to_string(),
                    arguments: "{}".to_string(),
                }],
            },
            vec![ToolChatMessage::Tool {
                tool_call_id: "call-other".to_string(),
                content: "错误结果".to_string(),
            }],
        );

        assert!(result.is_err());
    }
}
