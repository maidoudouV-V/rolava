use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::config::AppConfig;
use crate::repository::db_manager::QQChatContextManager;
use crate::transport::message::{ConversationKind, IncomingMessage, MessageTarget};
use crate::transport::MessageSender;

use super::chat_processor::ChatProcessor;
use super::conversation_actor::{ConversationActor, ConversationEvent};
use super::filter::ConversationFilter;

/// 根据会话 key 把平台消息投递给对应会话 Actor。
pub struct ConversationDispatcher {
    app_config: Arc<AppConfig>,
    db_manager: Arc<QQChatContextManager>,
    message_sender: Arc<dyn MessageSender>,
    message_rx: mpsc::Receiver<IncomingMessage>,
    actors: HashMap<String, mpsc::UnboundedSender<ConversationEvent>>,
}

impl ConversationDispatcher {
    pub fn new(
        app_config: Arc<AppConfig>,
        db_manager: Arc<QQChatContextManager>,
        message_sender: Arc<dyn MessageSender>,
        message_rx: mpsc::Receiver<IncomingMessage>,
    ) -> Self {
        Self {
            app_config,
            db_manager,
            message_sender,
            message_rx,
            actors: HashMap::new(),
        }
    }

    pub async fn run(&mut self) {
        while let Some(incoming_message) = self.message_rx.recv().await {
            let conversation_key = Self::conversation_key(&incoming_message);
            let actor_tx = self.get_or_spawn_actor(&conversation_key, &incoming_message);
            if actor_tx
                .send(ConversationEvent::IncomingMessage(incoming_message))
                .is_err()
            {
                eprintln!("会话 Actor 已关闭，消息分发失败: {}", conversation_key);
                self.actors.remove(&conversation_key);
            }
        }
    }

    fn get_or_spawn_actor(
        &mut self,
        conversation_key: &str,
        incoming_message: &IncomingMessage,
    ) -> mpsc::UnboundedSender<ConversationEvent> {
        if let Some(actor_tx) = self.actors.get(conversation_key) {
            return actor_tx.clone();
        }

        // 忙会话只积压自己的事件，不能阻塞其他会话的分发。
        let (actor_tx, actor_rx) = mpsc::unbounded_channel();
        let filter = ConversationFilter::new(self.app_config.clone(), self.db_manager.clone());
        let processor = ChatProcessor::new(
            self.db_manager.clone(),
            self.app_config.clone(),
            conversation_key.to_string(),
            Self::scene_name(&incoming_message.conversation.kind).to_string(),
            self.message_sender.clone(),
            MessageTarget::from(incoming_message),
        );
        let actor = ConversationActor::new(actor_rx, filter, processor);
        tokio::spawn(actor.run());
        self.actors
            .insert(conversation_key.to_string(), actor_tx.clone());
        actor_tx
    }

    fn conversation_key(incoming_message: &IncomingMessage) -> String {
        let kind = match incoming_message.conversation.kind {
            ConversationKind::Direct => "direct",
            ConversationKind::Group => "group",
        };
        format!(
            "{}:{}:{}",
            incoming_message.source, kind, incoming_message.conversation.id
        )
    }

    fn scene_name(kind: &ConversationKind) -> &'static str {
        match kind {
            ConversationKind::Direct => "私聊",
            ConversationKind::Group => "群聊",
        }
    }
}
