use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

use crate::tools::ToolRegistry;
use crate::transport::message::IncomingMessage;

use super::chat_processor::ChatProcessor;
use super::filter::ConversationFilter;

const MESSAGE_BATCH_WAIT: Duration = Duration::from_secs(2);
const MESSAGE_BATCH_MAX_MESSAGES: usize = 5;

/// 延时工具到期后，用于重新唤醒当前会话 Actor 的内部触发。
pub struct InternalTrigger {
    pub conversation_messages: Vec<IncomingMessage>,
}

/// 单个会话 mailbox 接收的全部事件类型。
pub enum ConversationEvent {
    IncomingMessage(IncomingMessage),
    InternalTrigger(InternalTrigger),
}

/// 单个会话的 Actor；mailbox 保证该会话的所有事件严格串行处理。
pub struct ConversationActor {
    event_rx: mpsc::UnboundedReceiver<ConversationEvent>,
    filter: ConversationFilter,
    processor: ChatProcessor,
    tools: ToolRegistry,
    /// 后续由聊天工具修改；为 true 时当前会话跳过 AI 前置过滤。
    bypass_ai_filter: bool,
}

impl ConversationActor {
    pub fn new(
        event_rx: mpsc::UnboundedReceiver<ConversationEvent>,
        filter: ConversationFilter,
        processor: ChatProcessor,
    ) -> Self {
        Self {
            event_rx,
            filter,
            processor,
            tools: ToolRegistry::built_in(),
            bypass_ai_filter: false,
        }
    }

    pub async fn run(mut self) {
        let mut pending_event = None;
        loop {
            let event = match pending_event.take() {
                Some(event) => event,
                None => match self.event_rx.recv().await {
                    Some(event) => event,
                    None => break,
                },
            };

            match event {
                ConversationEvent::IncomingMessage(incoming_message) => {
                    let (incoming_messages, next_event) =
                        Self::collect_message_batch(&mut self.event_rx, incoming_message).await;
                    pending_event = next_event;

                    let incoming_messages = self
                        .filter
                        .process_messages(incoming_messages, self.bypass_ai_filter)
                        .await;
                    if incoming_messages.is_empty() {
                        continue;
                    }
                    self.processor
                        .process_messages(incoming_messages, &self.tools)
                        .await;
                }
                ConversationEvent::InternalTrigger(trigger) => {
                    self.processor
                        .process_internal_trigger(trigger.conversation_messages, &self.tools)
                        .await;
                }
            }
        }
    }

    /// 收集当前会话的连续平台消息；每收到一条就重新等待两秒，最多五条。
    async fn collect_message_batch(
        event_rx: &mut mpsc::UnboundedReceiver<ConversationEvent>,
        first_message: IncomingMessage,
    ) -> (Vec<IncomingMessage>, Option<ConversationEvent>) {
        let mut messages = vec![first_message];

        while messages.len() < MESSAGE_BATCH_MAX_MESSAGES {
            match timeout(MESSAGE_BATCH_WAIT, event_rx.recv()).await {
                Ok(Some(ConversationEvent::IncomingMessage(message))) => messages.push(message),
                Ok(Some(event @ ConversationEvent::InternalTrigger(_))) => {
                    return (messages, Some(event));
                }
                Ok(None) | Err(_) => break,
            }
        }

        (messages, None)
    }
}
