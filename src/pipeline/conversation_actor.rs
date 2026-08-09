use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};
use tracing::{debug, info};

use crate::commands::{CommandOutput, CommandRuntimeAction, CommandSystem};
use crate::conversation_control::ConversationControl;
use crate::conversation_trigger::ConversationTrigger;
use crate::tools::ToolRegistry;
use crate::transport::message::{IncomingMessage, MessageTarget};

use super::chat_processor::ChatProcessor;
use super::filter::ConversationFilter;

const MESSAGE_BATCH_WAIT: Duration = Duration::from_secs(2);
const MESSAGE_BATCH_MAX_MESSAGES: usize = 5;

/// 单个会话 mailbox 接收的全部事件类型。
pub enum ConversationEvent {
    IncomingMessage(IncomingMessage),
    InternalTrigger(ConversationTrigger),
}

/// 单个会话的 Actor；mailbox 保证该会话的所有事件严格串行处理。
pub struct ConversationActor {
    event_rx: mpsc::UnboundedReceiver<ConversationEvent>,
    filter: ConversationFilter,
    processor: ChatProcessor,
    tools: ToolRegistry,
    commands: Arc<CommandSystem>,
    conversation_control: Arc<ConversationControl>,
}

impl ConversationActor {
    pub fn new(
        event_rx: mpsc::UnboundedReceiver<ConversationEvent>,
        filter: ConversationFilter,
        processor: ChatProcessor,
        commands: Arc<CommandSystem>,
        conversation_control: Arc<ConversationControl>,
    ) -> Self {
        Self {
            event_rx,
            filter,
            processor,
            tools: ToolRegistry::built_in(),
            commands,
            conversation_control,
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
                    if self.process_command(&incoming_message).await {
                        continue;
                    }
                    let (incoming_messages, next_event) = Self::collect_message_batch(
                        &mut self.event_rx,
                        self.commands.as_ref(),
                        incoming_message,
                    )
                    .await;
                    pending_event = next_event;
                    debug!(message_count = incoming_messages.len(), "会话消息聚合完成");

                    let incoming_messages = self.filter.process_messages(incoming_messages).await;
                    if incoming_messages.is_empty() {
                        debug!("本批消息未进入主 AI 处理");
                        continue;
                    }
                    self.processor
                        .process_messages(incoming_messages, &self.tools)
                        .await;
                }
                ConversationEvent::InternalTrigger(trigger) => {
                    self.processor
                        .process_internal_trigger(trigger, &self.tools)
                        .await;
                }
            }
        }
        info!("会话 Actor 已停止");
    }

    /// 白名单命令在普通消息入库和 AI 过滤之前执行。
    async fn process_command(&mut self, incoming_message: &IncomingMessage) -> bool {
        let Some(output) = self.commands.execute_if_command(incoming_message).await else {
            return false;
        };

        self.apply_command_output(&output);
        self.commands
            .send_reply(
                &MessageTarget::from(incoming_message),
                output.reply.as_deref(),
            )
            .await;
        true
    }

    fn apply_command_output(&mut self, output: &CommandOutput) {
        match output.runtime_action {
            CommandRuntimeAction::None => {}
            CommandRuntimeAction::ResetConversation => {
                // 数据库已由命令清空；这里统一丢弃 Actor 内仍引用旧会话的临时状态。
                self.filter.reset_conversation_state();
                self.processor.reset_conversation_state();
                self.conversation_control.set_ai_filter_bypassed(false);
                self.tools.reset_conversation_state();
            }
        }
    }

    /// 收集当前会话的连续平台消息；每收到一条就重新等待两秒，最多五条。
    async fn collect_message_batch(
        event_rx: &mut mpsc::UnboundedReceiver<ConversationEvent>,
        commands: &CommandSystem,
        first_message: IncomingMessage,
    ) -> (Vec<IncomingMessage>, Option<ConversationEvent>) {
        let mut messages = vec![first_message];

        while messages.len() < MESSAGE_BATCH_MAX_MESSAGES {
            match timeout(MESSAGE_BATCH_WAIT, event_rx.recv()).await {
                Ok(Some(ConversationEvent::IncomingMessage(message))) => {
                    if commands.is_command_message(&message) {
                        // 命令是聚合边界，先处理已经收集的普通消息，再单独执行命令。
                        return (messages, Some(ConversationEvent::IncomingMessage(message)));
                    }
                    messages.push(message);
                }
                Ok(Some(event @ ConversationEvent::InternalTrigger(_))) => {
                    return (messages, Some(event));
                }
                Ok(None) | Err(_) => break,
            }
        }

        (messages, None)
    }
}
