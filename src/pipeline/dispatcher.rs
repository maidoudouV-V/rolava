use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, error, info, info_span, Instrument};

use crate::commands::CommandSystem;
use crate::config::AppConfig;
use crate::conversation_control::ConversationControl;
use crate::conversation_trigger::{
    ConversationTrigger, ConversationTriggerSender, RoutedConversationTrigger,
};
use crate::message_ingestion::MessageIngestionService;
use crate::repository::db_manager::QQChatContextManager;
use crate::scheduler::SchedulerService;
use crate::tools::ToolServices;
use crate::transport::message::{ConversationKind, IncomingMessage, MessageTarget};
use crate::transport::MessageSender;

use super::chat_processor::ChatProcessor;
use super::conversation_actor::{ConversationActor, ConversationEvent};
use super::filter::ConversationFilter;

struct ActorConversationTriggerSender {
    actor_tx: mpsc::UnboundedSender<ConversationEvent>,
}

impl ConversationTriggerSender for ActorConversationTriggerSender {
    fn send_trigger(&self, trigger: ConversationTrigger) -> anyhow::Result<()> {
        self.actor_tx
            .send(ConversationEvent::InternalTrigger(trigger))
            .map_err(|_| anyhow::anyhow!("会话 Actor 已关闭，内部触发发送失败"))
    }
}

/// 根据会话 key 把平台消息投递给对应会话 Actor。
pub struct ConversationDispatcher {
    tool_services: Arc<ToolServices>,
    message_ingestion: Arc<MessageIngestionService>,
    command_system: Arc<CommandSystem>,
    message_rx: mpsc::Receiver<IncomingMessage>,
    internal_trigger_rx: mpsc::UnboundedReceiver<RoutedConversationTrigger>,
    actors: HashMap<String, mpsc::UnboundedSender<ConversationEvent>>,
}

impl ConversationDispatcher {
    pub fn new(
        app_config: Arc<AppConfig>,
        db_manager: Arc<QQChatContextManager>,
        message_sender: Arc<dyn MessageSender>,
        scheduler: Arc<SchedulerService>,
        message_ingestion: Arc<MessageIngestionService>,
        message_rx: mpsc::Receiver<IncomingMessage>,
        internal_trigger_rx: mpsc::UnboundedReceiver<RoutedConversationTrigger>,
    ) -> Self {
        let command_system = Arc::new(CommandSystem::built_in(
            db_manager.clone(),
            message_sender.clone(),
            app_config.app.command_whitelist.clone(),
        ));
        Self {
            tool_services: Arc::new(ToolServices::new(
                app_config,
                db_manager,
                message_sender,
                scheduler,
            )),
            message_ingestion,
            command_system,
            message_rx,
            internal_trigger_rx,
            actors: HashMap::new(),
        }
    }

    pub async fn run(&mut self) {
        loop {
            // 平台消息和应用内部触发在同一入口按会话路由，之后由 Actor 保证串行。
            tokio::select! {
                incoming_message = self.message_rx.recv() => {
                    let Some(incoming_message) = incoming_message else {
                        break;
                    };
                    let target = MessageTarget::from(&incoming_message);
                    self.dispatch_event(
                        target,
                        ConversationEvent::IncomingMessage(incoming_message),
                    );
                }
                routed_trigger = self.internal_trigger_rx.recv() => {
                    let Some(routed_trigger) = routed_trigger else {
                        break;
                    };
                    self.dispatch_event(
                        routed_trigger.target,
                        ConversationEvent::InternalTrigger(routed_trigger.trigger),
                    );
                }
            }
        }
    }

    fn dispatch_event(&mut self, target: MessageTarget, event: ConversationEvent) {
        let conversation_key = Self::conversation_key(&target);
        let actor_tx = self.get_or_spawn_actor(&conversation_key, &target);
        if let Err(send_error) = actor_tx.send(event) {
            // Actor 异常退出时移除失效邮箱并重建一次，避免当前事件直接丢失。
            self.actors.remove(&conversation_key);
            let replacement = self.get_or_spawn_actor(&conversation_key, &target);
            if replacement.send(send_error.0).is_err() {
                error!(conversation_key, "重建会话 Actor 后事件仍然分发失败");
                self.actors.remove(&conversation_key);
            }
        }
    }

    fn get_or_spawn_actor(
        &mut self,
        conversation_key: &str,
        target: &MessageTarget,
    ) -> mpsc::UnboundedSender<ConversationEvent> {
        if let Some(actor_tx) = self.actors.get(conversation_key) {
            return actor_tx.clone();
        }

        // 忙会话只积压自己的事件，不能阻塞其他会话的分发。
        let (actor_tx, actor_rx) = mpsc::unbounded_channel();
        let trigger_sender: Arc<dyn ConversationTriggerSender> =
            Arc::new(ActorConversationTriggerSender {
                actor_tx: actor_tx.clone(),
            });
        let conversation_control = Arc::new(ConversationControl::default());
        let filter = ConversationFilter::new(
            self.tool_services.app_config.clone(),
            self.tool_services.db_manager.clone(),
            self.message_ingestion.clone(),
            conversation_control.clone(),
        );
        let processor = ChatProcessor::new(
            self.tool_services.clone(),
            conversation_key.to_string(),
            Self::scene_name(&target.conversation.kind).to_string(),
            target.clone(),
            conversation_control.clone(),
            trigger_sender,
        );
        let actor = ConversationActor::new(
            actor_rx,
            filter,
            processor,
            self.command_system.clone(),
            conversation_control,
        );
        let actor_span = info_span!("conversation", conversation_key);
        tokio::spawn(actor.run().instrument(actor_span));
        self.actors
            .insert(conversation_key.to_string(), actor_tx.clone());
        info!(conversation_key, "已创建会话 Actor");
        debug!(conversation_key, "会话 Actor 已加入分发表");
        actor_tx
    }

    fn conversation_key(target: &MessageTarget) -> String {
        let kind = match target.conversation.kind {
            ConversationKind::Direct => "direct",
            ConversationKind::Group => "group",
        };
        format!("{}:{}:{}", target.source, kind, target.conversation.id)
    }

    fn scene_name(kind: &ConversationKind) -> &'static str {
        match kind {
            ConversationKind::Direct => "私聊",
            ConversationKind::Group => "群聊",
        }
    }
}
