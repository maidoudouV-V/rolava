use std::sync::Arc;
use tracing::{debug, error, info, warn};

use crate::ai_provider::{ContextMessage, MessageRole, ToolChatMessage, ToolChatResponse};
use crate::config::AppConfig;
use crate::conversation_control::ConversationControl;
use crate::message_enricher::MessageEnricher;
use crate::repository::db_manager::{ChatMessage, QQChatContextManager};
use crate::transport::message::{ConversationKind, IncomingMessage};

const INITIAL_FILTER_CONTEXT_MESSAGES: u32 = 50;
const MAX_FILTER_CONTEXT_MESSAGES: usize = 100;
const FILTER_CONTEXT_DROP_MESSAGES: usize = 50;

#[derive(Debug, PartialEq, Eq)]
enum FilterAction {
    Reply,
    Ignore,
}

/// 已经完成增强、入库和基础过滤的平台消息。
pub struct FilteredMessage {
    /// 增强后的标准平台消息。
    pub message: IncomingMessage,
    /// 对应 messages 表的数据库主键。
    pub database_id: i64,
}

/// 单个会话 Actor 独立持有的消息过滤器。
pub struct ConversationFilter {
    app_config: Arc<AppConfig>,
    db_manager: Arc<QQChatContextManager>,
    message_enricher: MessageEnricher,
    conversation_control: Arc<ConversationControl>,
    filter_context: Vec<ContextMessage>,
    filter_context_initialized: bool,
}

impl ConversationFilter {
    pub fn new(
        app_config: Arc<AppConfig>,
        db_manager: Arc<QQChatContextManager>,
        conversation_control: Arc<ConversationControl>,
    ) -> Self {
        Self {
            app_config: app_config.clone(),
            db_manager: db_manager.clone(),
            message_enricher: MessageEnricher::new(app_config, db_manager),
            conversation_control,
            filter_context: Vec::new(),
            filter_context_initialized: false,
        }
    }

    /// 清除过滤模型的当前会话缓存；下一条普通消息会从数据库重新初始化。
    pub fn reset_conversation_state(&mut self) {
        self.filter_context.clear();
        self.filter_context_initialized = false;
    }

    /// 批量完成基础过滤、消息增强和入库，只返回可以进入后续处理的消息。
    pub async fn process_messages(
        &mut self,
        incoming_messages: Vec<IncomingMessage>,
    ) -> Vec<FilteredMessage> {
        let mut accepted_messages = Vec::with_capacity(incoming_messages.len());

        for incoming_message in incoming_messages {
            if !self.should_accept_platform_message(&incoming_message) {
                continue;
            }

            let incoming_message = self.message_enricher.enrich(incoming_message).await;
            match self.db_manager.write_incoming_message(&incoming_message) {
                Ok(stored_message) => accepted_messages.push(FilteredMessage {
                    message: incoming_message,
                    database_id: stored_message.id,
                }),
                Err(err) => {
                    error!(error = %err, "写入聊天消息失败，不进入后续处理");
                    continue;
                }
            }
        }

        if accepted_messages.is_empty() {
            return accepted_messages;
        }

        if !self.ai_filter_enabled_for(&accepted_messages) {
            return accepted_messages;
        }

        if let Err(err) = self.update_filter_context(&accepted_messages) {
            warn!(error = %err, "构造消息过滤上下文失败，继续主处理");
            return accepted_messages;
        }

        if self.conversation_control.ai_filter_bypassed()
            || accepted_messages
                .iter()
                .any(|message| Self::mentions_bot(&message.message))
        {
            return accepted_messages;
        }

        debug!(
            messages = %Self::render_current_messages_log(&accepted_messages),
            "AI 前置过滤当前消息"
        );
        match self.request_filter_model().await {
            Ok(response) => {
                debug!(response = ?response.content, "AI 前置过滤原始结果");
                match response
                    .content
                    .as_deref()
                    .and_then(Self::parse_filter_action)
                {
                    Some(FilterAction::Reply) => {
                        info!(filter_action = "reply", "AI 前置过滤完成");
                        self.conversation_control.set_ai_filter_bypassed(true);
                        accepted_messages
                    }
                    Some(FilterAction::Ignore) => {
                        info!(filter_action = "ignore", "AI 前置过滤完成");
                        Vec::new()
                    }
                    None => {
                        warn!("无法识别消息过滤结果，继续后续处理");
                        accepted_messages
                    }
                }
            }
            Err(err) => {
                warn!(error = %err, "消息过滤模型请求失败，继续后续处理");
                accepted_messages
            }
        }
    }

    /// 首次加载最近 50 条历史，后续只追加新消息；达到 100 条时淘汰最老 50 条。
    fn update_filter_context(
        &mut self,
        incoming_messages: &[FilteredMessage],
    ) -> anyhow::Result<()> {
        if !self.filter_context_initialized {
            let conversation = incoming_messages
                .last()
                .expect("非空消息批次必须包含最后一条消息")
                .message
                .clone();
            let history = self.db_manager.get_latest_conversation_history(
                &conversation.source,
                &conversation.conversation.id,
                INITIAL_FILTER_CONTEXT_MESSAGES,
            )?;
            self.filter_context = history
                .iter()
                .map(|message| Self::history_context_message(message, &conversation.bot_id))
                .collect();
            self.filter_context_initialized = true;
        } else {
            self.filter_context.extend(
                incoming_messages
                    .iter()
                    .map(|message| Self::incoming_context_message(&message.message)),
            );
        }

        Self::trim_filter_context(&mut self.filter_context);

        Ok(())
    }

    fn trim_filter_context(context: &mut Vec<ContextMessage>) {
        if context.len() >= MAX_FILTER_CONTEXT_MESSAGES {
            let drop_count = FILTER_CONTEXT_DROP_MESSAGES.min(context.len());
            context.drain(..drop_count);
        }
    }

    /// 固定提示词位于首位，其余内容完全来自当前会话缓存。
    fn build_filter_messages(&self) -> Vec<ToolChatMessage> {
        let mut messages = Vec::with_capacity(self.filter_context.len() + 1);
        messages.push(ToolChatMessage::System {
            content: self.app_config.prompt_config.filter_prompt.clone(),
        });
        messages.extend(self.filter_context.iter().map(ToolChatMessage::from));
        messages
    }

    async fn request_filter_model(&self) -> anyhow::Result<ToolChatResponse> {
        let messages = self.build_filter_messages();
        let provider = self
            .app_config
            .ai_models
            .get(&self.app_config.app.filter_model_name)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "找不到消息过滤模型配置: {}",
                    self.app_config.app.filter_model_name
                )
            })?;
        provider.chat_completions(&messages, &[]).await
    }

    fn parse_filter_action(content: &str) -> Option<FilterAction> {
        match content.trim().to_ascii_lowercase().as_str() {
            "reply" => Some(FilterAction::Reply),
            "ignore" => Some(FilterAction::Ignore),
            _ => None,
        }
    }

    fn render_current_messages_log(messages: &[FilteredMessage]) -> String {
        messages
            .iter()
            .map(|message| {
                format!(
                    "{}: {}",
                    message.message.sender.display_name, message.message.content.text
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn ai_filter_enabled_for(&self, messages: &[FilteredMessage]) -> bool {
        self.app_config.app.enable_ai_filter
            && messages.first().is_some_and(|message| {
                matches!(message.message.conversation.kind, ConversationKind::Group)
            })
    }

    fn mentions_bot(message: &IncomingMessage) -> bool {
        message.content.parts.iter().any(|part| {
            if part.kind != "at" {
                return false;
            }
            let Some(mentioned_id) = part.data.get("qq") else {
                return false;
            };
            mentioned_id
                .as_str()
                .map(str::to_string)
                .or_else(|| mentioned_id.as_i64().map(|id| id.to_string()))
                .or_else(|| mentioned_id.as_u64().map(|id| id.to_string()))
                .is_some_and(|id| id == message.bot_id)
        })
    }

    fn history_context_message(message: &ChatMessage, bot_id: &str) -> ContextMessage {
        let content = message.content_text.clone().unwrap_or_default();
        if message.sender_id == bot_id {
            ContextMessage {
                role: MessageRole::Assistant,
                content,
            }
        } else {
            let sender_name = message
                .sender_nickname
                .as_ref()
                .unwrap_or(&message.sender_display_name);
            ContextMessage {
                role: MessageRole::User,
                content: format!("{}: {}", sender_name, content),
            }
        }
    }

    fn incoming_context_message(message: &IncomingMessage) -> ContextMessage {
        ContextMessage {
            role: MessageRole::User,
            content: format!("{}: {}", message.sender.display_name, message.content.text),
        }
    }

    /// 基础接入过滤：忽略机器人自身消息以及白名单之外的消息。
    fn should_accept_platform_message(&self, incoming_message: &IncomingMessage) -> bool {
        if incoming_message.sender.id == incoming_message.bot_id {
            return false;
        }
        if !self.is_message_allowed(incoming_message) {
            debug!(
                source = %incoming_message.source,
                conversation_id = %incoming_message.conversation.id,
                sender_id = %incoming_message.sender.id,
                "跳过非白名单消息"
            );
            return false;
        }
        true
    }

    /// 判断消息是否通过私聊或群聊白名单；白名单为空时放行对应类型的所有消息。
    fn is_message_allowed(&self, incoming_message: &IncomingMessage) -> bool {
        match incoming_message.conversation.kind {
            ConversationKind::Direct => {
                self.app_config.app.direct_whitelist.is_empty()
                    || self
                        .app_config
                        .app
                        .direct_whitelist
                        .contains(&incoming_message.sender.id)
            }
            ConversationKind::Group => {
                self.app_config.app.group_whitelist.is_empty()
                    || self
                        .app_config
                        .app
                        .group_whitelist
                        .contains(&incoming_message.conversation.id)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::{ConversationFilter, FilterAction, MAX_FILTER_CONTEXT_MESSAGES};
    use crate::ai_provider::{ContextMessage, MessageRole};
    use crate::transport::message::{
        Conversation, ConversationKind, IncomingMessage, MessageContent, MessagePart, Participant,
    };

    #[test]
    fn filter_context_keeps_prefix_until_it_reaches_one_hundred_messages() {
        let mut context = messages(MAX_FILTER_CONTEXT_MESSAGES - 1);

        ConversationFilter::trim_filter_context(&mut context);

        assert_eq!(context.len(), 99);
        assert_eq!(context[0].content, "0");
    }

    #[test]
    fn filter_context_drops_oldest_fifty_at_one_hundred_messages() {
        let mut context = messages(MAX_FILTER_CONTEXT_MESSAGES);

        ConversationFilter::trim_filter_context(&mut context);

        assert_eq!(context.len(), 50);
        assert_eq!(context[0].content, "50");
        assert_eq!(context[49].content, "99");
    }

    #[test]
    fn parses_plain_text_filter_actions() {
        assert_eq!(
            ConversationFilter::parse_filter_action(" reply\n"),
            Some(FilterAction::Reply)
        );
        assert_eq!(
            ConversationFilter::parse_filter_action("IGNORE"),
            Some(FilterAction::Ignore)
        );
        assert_eq!(ConversationFilter::parse_filter_action("unknown"), None);
    }

    #[test]
    fn only_mentions_of_current_bot_bypass_ai_filter() {
        let bot_mention = message_with_mention("10001", json!({ "qq": "10001" }));
        let other_mention = message_with_mention("10001", json!({ "qq": "20002" }));

        assert!(ConversationFilter::mentions_bot(&bot_mention));
        assert!(!ConversationFilter::mentions_bot(&other_mention));
    }

    fn messages(count: usize) -> Vec<ContextMessage> {
        (0..count)
            .map(|index| ContextMessage {
                role: MessageRole::User,
                content: index.to_string(),
            })
            .collect()
    }

    fn message_with_mention(bot_id: &str, mention_data: Value) -> IncomingMessage {
        IncomingMessage {
            source: "test".to_string(),
            bot_id: bot_id.to_string(),
            conversation: Conversation {
                id: "group".to_string(),
                kind: ConversationKind::Group,
                title: None,
            },
            sender: Participant {
                id: "sender".to_string(),
                display_name: "sender".to_string(),
                nickname: None,
                role: None,
            },
            content: MessageContent {
                text: String::new(),
                parts: vec![MessagePart {
                    kind: "at".to_string(),
                    data: mention_data,
                }],
            },
            message_id: None,
            timestamp: 0,
            metadata: Value::Null,
        }
    }
}
