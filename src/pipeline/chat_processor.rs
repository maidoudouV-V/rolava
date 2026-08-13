use crate::ai_provider::{ToolChatMessage, ToolChatResponse, ToolChatUserContent};
use chrono::{DateTime, Local, Utc};
use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use tokio::fs;
use tokio::sync::OnceCell;
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info, info_span, trace, warn, Instrument};

use crate::conversation_context::{ActiveToolHistory, RuntimeContextState, ToolRoundHistory};
use crate::conversation_control::ConversationControl;
use crate::conversation_trigger::{ConversationTrigger, ConversationTriggerSender};
use crate::memory::{CharacterMemorySession, UserMemorySession};
use crate::message_enricher::MessageEnricher;
use crate::repository::db_manager::ChatMessage;
use crate::tools::{
    ConversationEffect, ConversationToolContext, ToolContext, ToolDefinition, ToolRegistry,
    ToolResult, ToolServices,
};
use crate::transport::message::{ConversationKind, IncomingMessage, MessageTarget};
use crate::transport::{GroupInfo, SendOptions};

use super::filter::FilteredMessage;

const MAX_TOOL_ROUNDS: usize = 8;
const DIRECT_TOOL_HISTORY_TURNS: usize = 5;

/// 为单个会话构造上下文、请求 AI 并处理响应。
pub struct ChatProcessor {
    services: Arc<ToolServices>,
    conversation_key: String,
    scene: String,
    message_target: MessageTarget,
    conversation_control: Arc<ConversationControl>,
    trigger_sender: Arc<dyn ConversationTriggerSender>,
    character_memory: Arc<CharacterMemorySession>,
    user_memory: Arc<UserMemorySession>,
    group_info: OnceCell<Option<GroupInfo>>,
    runtime_context: RuntimeContextState,
}

struct BuiltContext {
    messages: Vec<ToolChatMessage>,
    unread_message_ids: Vec<i64>,
    unread_message_time: Option<String>,
    pending_expired_character_memory_ids: Vec<i64>,
    latest_message_id: Option<i64>,
}

struct RenderedPrompt {
    content: String,
    pending_expired_character_memory_ids: Vec<i64>,
}

impl ChatProcessor {
    /// 创建一个只服务于指定会话的聊天处理器。
    pub fn new(
        services: Arc<ToolServices>,
        conversation_key: String,
        scene: String,
        message_target: MessageTarget,
        conversation_control: Arc<ConversationControl>,
        trigger_sender: Arc<dyn ConversationTriggerSender>,
    ) -> Self {
        let user_memory = Arc::new(UserMemorySession::new(
            message_target.clone(),
            services.app_config.as_ref(),
            services.db_manager.clone(),
        ));
        let character_memory = Arc::new(CharacterMemorySession::new(
            message_target.clone(),
            services.db_manager.clone(),
        ));
        Self {
            services,
            conversation_key,
            scene,
            message_target,
            conversation_control,
            trigger_sender,
            character_memory,
            user_memory,
            group_info: OnceCell::new(),
            runtime_context: RuntimeContextState::default(),
        }
    }

    pub fn reset_conversation_state(&mut self) {
        self.user_memory.reset();
        self.runtime_context.reset();
    }

    /// 处理平台发来的消息。过滤和入库已由当前会话 Actor 在调用前完成。
    pub async fn process_messages(
        &mut self,
        filtered_messages: Vec<FilteredMessage>,
        tools: &ToolRegistry,
    ) {
        // Actor 在过滤完成后立即进入这里，回复延时从此刻开始计算。
        let send_options = SendOptions::delay_started_now();
        let current_message_ids = filtered_messages
            .iter()
            .map(|message| message.database_id)
            .collect::<Vec<_>>();
        let incoming_messages = filtered_messages
            .into_iter()
            .map(|message| message.message)
            .collect::<Vec<_>>();
        info!(message_count = incoming_messages.len(), "开始处理平台消息");
        for incoming_message in &incoming_messages {
            debug!(
                scene = %self.scene,
                sender = %incoming_message.sender.display_name,
                content = ?incoming_message.content.text,
                "收到平台消息"
            );
        }
        self.process_conversation(
            &incoming_messages,
            tools,
            None,
            send_options,
            &current_message_ids,
        )
        .await;
    }

    /// 处理工具产生的内部触发；临时提示只参与本次请求，不写入数据库。
    pub async fn process_internal_trigger(
        &mut self,
        trigger: ConversationTrigger,
        tools: &ToolRegistry,
    ) {
        let send_options = SendOptions::delay_started_now();
        let ConversationTrigger { user_prompt } = trigger;
        info!("收到内部会话触发");
        trace!(prompt = %user_prompt, "内部会话触发完整提示");
        self.process_conversation(&[], tools, Some(&user_prompt), send_options, &[])
            .await;
    }

    async fn process_conversation(
        &mut self,
        conversation_messages: &[IncomingMessage],
        tools: &ToolRegistry,
        transient_user_prompt: Option<&str>,
        send_options: SendOptions,
        current_message_ids: &[i64],
    ) {
        if matches!(
            self.message_target.conversation.kind,
            ConversationKind::Direct
        ) {
            let removed = self.runtime_context.advance_tool_history_turn();
            if removed > 0 {
                debug!(tool_history_count = removed, "私聊工具历史已超过五轮保留期");
            }
        }
        let built_context = match self
            .build_context(
                conversation_messages,
                current_message_ids,
                transient_user_prompt,
            )
            .await
        {
            Ok(context) => context,
            Err(err) => {
                error!(error = %err, "构造聊天上下文失败");
                return;
            }
        };

        let unread_message_time = built_context.unread_message_time.clone();
        let mut request_messages = built_context.messages;
        let mut pending_expired_character_memory_ids =
            built_context.pending_expired_character_memory_ids;
        let mut displayed_expired_character_memory_ids = HashSet::new();
        let tool_definitions = tools.definitions();
        let tool_context = ToolContext {
            conversation: ConversationToolContext {
                key: self.conversation_key.clone(),
                target: self.message_target.clone(),
                current_messages: conversation_messages.to_vec(),
                control: self.conversation_control.clone(),
                trigger_sender: self.trigger_sender.clone(),
                character_memory: self.character_memory.clone(),
                user_memory: self.user_memory.clone(),
            },
            services: self.services.clone(),
        };
        let mut messages_marked_read = false;
        let mut tool_round_history = Vec::new();
        let mut tool_round_message_ids = Vec::new();
        let mut conversation_effect = ConversationEffect::None;

        for tool_round in 0..=MAX_TOOL_ROUNDS {
            let ai_span = info_span!(
                "ai_request",
                round = tool_round + 1,
                message_count = request_messages.len(),
                tool_count = tool_definitions.len()
            );
            let response = match self
                .request_ai(&request_messages, &tool_definitions)
                .instrument(ai_span)
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    error!(error = %err, "AI 请求最终失败");
                    break;
                }
            };
            displayed_expired_character_memory_ids
                .extend(pending_expired_character_memory_ids.iter().copied());

            if !messages_marked_read {
                self.runtime_context.seal_current_tail();
                if let Err(err) = self
                    .services
                    .db_manager
                    .mark_messages_read(&built_context.unread_message_ids)
                {
                    error!(error = %err, "更新消息已读状态失败");
                }
                messages_marked_read = true;
            }

            let visible_content = Self::non_empty_response_content(response.content.as_deref());
            let mut emitted_message_ids = Vec::new();
            if let Some(content) = visible_content {
                match self
                    .services
                    .message_sender
                    .send_text(&self.message_target, content, send_options)
                    .await
                {
                    Ok(sent_messages) => emitted_message_ids
                        .extend(sent_messages.into_iter().map(|message| message.database_id)),
                    Err(err) => {
                        error!(error = %err, "发送 AI 回复失败")
                    }
                }
            }

            let assistant_message = response.assistant_message();
            let tool_calls = response.tool_calls;
            if tool_calls.is_empty() {
                break;
            }

            tool_round_message_ids.extend(emitted_message_ids);

            if tool_round == MAX_TOOL_ROUNDS {
                warn!(
                    max_tool_rounds = MAX_TOOL_ROUNDS,
                    "连续工具调用达到上限，停止继续请求 AI"
                );
                let tool_results = tool_calls
                    .iter()
                    .map(|tool_call| ToolChatMessage::Tool {
                        tool_call_id: tool_call.id.clone(),
                        content: format!(
                            "工具 {} 未执行：连续工具调用已达到最大轮数",
                            tool_call.name
                        ),
                    })
                    .collect();
                match ToolRoundHistory::new(assistant_message, tool_results) {
                    Ok(round) => tool_round_history.push(round),
                    Err(error) => error!(error = %error, "保存内存工具轮次失败"),
                }
                break;
            }

            let mut tool_results = Vec::new();
            for tool_call in tool_calls {
                let tool_arguments = tool_call.arguments.clone();
                info!(
                    tool_name = %tool_call.name,
                    tool_call_id = %tool_call.id,
                    "开始调用工具"
                );
                debug!(
                    tool_name = %tool_call.name,
                    tool_call_id = %tool_call.id,
                    arguments = %tool_arguments,
                    "工具调用参数"
                );
                let tool_span = info_span!(
                    "tool_call",
                    tool_name = %tool_call.name,
                    tool_call_id = %tool_call.id
                );
                let result = tools
                    .execute(&tool_context, tool_call)
                    .instrument(tool_span)
                    .await;
                if result.is_error {
                    warn!(
                        tool_name = %result.tool_name,
                        tool_call_id = %result.tool_call_id,
                        requires_ai_response = result.requires_ai_response,
                        arguments = %tool_arguments,
                        error = %result.content,
                        "工具调用失败"
                    );
                } else {
                    info!(
                        tool_name = %result.tool_name,
                        tool_call_id = %result.tool_call_id,
                        requires_ai_response = result.requires_ai_response,
                        "工具调用完成"
                    );
                }
                debug!(
                    tool_name = %result.tool_name,
                    tool_call_id = %result.tool_call_id,
                    content = %result.content,
                    "工具调用结果"
                );
                tool_results.push(result);
            }

            for result in &tool_results {
                if !result.is_error && result.conversation_effect != ConversationEffect::None {
                    conversation_effect = result.conversation_effect;
                }
            }

            let tool_result_messages = tool_results
                .iter()
                .map(ToolChatMessage::from)
                .collect::<Vec<_>>();
            match ToolRoundHistory::new(assistant_message.clone(), tool_result_messages.clone()) {
                Ok(round) => tool_round_history.push(round),
                Err(error) => error!(error = %error, "保存内存工具轮次失败"),
            }

            if !Self::should_continue_tool_loop(&tool_results) {
                debug!("本轮工具均不需要 AI 后续响应，结束工具循环");
                break;
            }

            if tool_results.iter().any(|result| {
                !result.is_error
                    && matches!(
                        result.tool_name.as_str(),
                        "set_character_memory"
                            | "delete_character_memory"
                            | "create_user_memory"
                            | "update_user_memory"
                            | "delete_user_memory"
                    )
            }) {
                match self.refresh_instruction_prompt(
                    &mut request_messages,
                    unread_message_time.as_deref(),
                ) {
                    Ok(pending_ids) => {
                        pending_expired_character_memory_ids = pending_ids;
                    }
                    Err(error) => {
                        error!(error = %error, "刷新记忆提示词失败");
                    }
                }
            }

            request_messages.push(assistant_message);
            request_messages.extend(tool_result_messages);
        }

        if !displayed_expired_character_memory_ids.is_empty() {
            let displayed_ids = displayed_expired_character_memory_ids
                .into_iter()
                .collect::<Vec<_>>();
            match self.character_memory.finish_turn(&displayed_ids) {
                Ok(marked) if marked > 0 => {
                    debug!(memory_count = marked, "已确认展示本轮未续期的到期角色记忆");
                }
                Ok(_) => {}
                Err(error) => error!(error = %error, "确认到期角色记忆展示状态失败"),
            }
        }

        if conversation_effect == ConversationEffect::End {
            let removed = self.runtime_context.compact_finished_conversation();
            debug!(tool_history_count = removed, "对话结束，已压缩内存工具历史");
        } else if !tool_round_history.is_empty() {
            match ActiveToolHistory::new(
                built_context.latest_message_id,
                tool_round_history,
                tool_round_message_ids,
            ) {
                Ok(history) => {
                    // 私聊减少长期工具协议占用，群聊则继续保留到对话结束或窗口淘汰。
                    let history = if matches!(
                        self.message_target.conversation.kind,
                        ConversationKind::Direct
                    ) {
                        history.with_turn_limit(DIRECT_TOOL_HISTORY_TURNS)
                    } else {
                        history
                    };
                    if let Err(error) = self.runtime_context.push_tool_history(history) {
                        error!(error = %error, "追加内存工具历史失败");
                    }
                }
                Err(error) => error!(error = %error, "构造内存工具历史失败"),
            }
        }
    }

    /// 任意工具要求 AI 后续处理时，回传本轮全部工具结果并继续循环。
    fn should_continue_tool_loop(tool_results: &[ToolResult]) -> bool {
        tool_results
            .iter()
            .any(|result| result.requires_ai_response)
    }

    /// 工具型响应可能返回空字符串；空白正文不能发送到消息平台。
    fn non_empty_response_content(content: Option<&str>) -> Option<&str> {
        content.filter(|content| !content.trim().is_empty())
    }

    /// 使用已经构造好的上下文请求聊天模型，并返回通用 tools 响应。
    async fn request_ai(
        &self,
        messages: &[ToolChatMessage],
        tool_definitions: &[ToolDefinition],
    ) -> anyhow::Result<ToolChatResponse> {
        let max_attempts = self.services.app_config.app.ai_request_max_attempts();
        let mut last_error = None;

        for attempt in 1..=max_attempts {
            let chat_result = {
                let chat_provider = self
                    .services
                    .app_config
                    .ai_models
                    .get(&self.services.app_config.app.chat_model_name)
                    .expect("找不到聊天模型配置");
                Self::run_ai_request_with_timeout(
                    self.services.app_config.app.ai_request_timeout_seconds,
                    "聊天模型 API 请求",
                    chat_provider.chat_completions(messages, tool_definitions),
                )
                .await
            };

            match chat_result {
                Ok(resp) => {
                    info!(
                        attempt,
                        model = ?resp.model,
                        finish_reason = ?resp.finish_reason,
                        tool_call_count = resp.tool_calls.len(),
                        has_content = resp.content.as_ref().is_some_and(|content| !content.is_empty()),
                        "AI 请求成功"
                    );
                    trace!(
                        reasoning = %resp.reasoning.display_text.as_deref().unwrap_or(""),
                        content = %resp.content.as_deref().unwrap_or(""),
                        "AI 解析后的响应内容"
                    );
                    return Ok(resp);
                }
                Err(err) => {
                    warn!(attempt, max_attempts, error = %err, "AI 请求失败，准备重试");
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.expect("AI 请求重试循环至少应执行一次"))
    }

    async fn run_ai_request_with_timeout<T, F>(
        timeout_seconds: u64,
        request_name: &str,
        request: F,
    ) -> anyhow::Result<T>
    where
        F: Future<Output = anyhow::Result<T>>,
    {
        if timeout_seconds == 0 {
            return request.await;
        }

        match timeout(Duration::from_secs(timeout_seconds), request).await {
            Ok(result) => result,
            Err(_) => anyhow::bail!("{}超时，超过 {} 秒", request_name, timeout_seconds),
        }
    }

    /// 构建发送给聊天模型的完整上下文，包括系统提示词、聊天历史和当前指令。
    async fn build_context(
        &mut self,
        current_messages: &[IncomingMessage],
        current_message_ids: &[i64],
        transient_user_prompt: Option<&str>,
    ) -> anyhow::Result<BuiltContext> {
        let deleted_memories = self.character_memory.begin_turn()?;
        if deleted_memories > 0 {
            debug!(memory_count = deleted_memories, "已删除确认遗忘的角色记忆");
        }
        let supports_vision = self.services.app_config.chat_model_supports_vision();
        let history_window = self.services.db_manager.get_conversation_history_window(
            &self.message_target.source,
            &self.message_target.conversation.id,
            self.services.app_config.app.max_history_messages,
        )?;
        let message_ids = history_window
            .messages
            .iter()
            .map(|message| message.id)
            .collect::<Vec<_>>();
        if self.runtime_context.reconcile_message_ids(&message_ids)? {
            debug!("聊天窗口已淘汰或删除旧记录，重新计算内存分块");
        }
        self.user_memory
            .refresh_active_users(
                &history_window.messages,
                current_messages,
                current_message_ids,
            )
            .await;
        self.ensure_group_info_loaded().await;
        let earliest_unread_timestamp = history_window
            .messages
            .iter()
            .find(|message| message.sender_id != self.message_target.bot_id && !message.is_read)
            .map(|message| message.event_timestamp);
        let unread_message_time = Self::render_unread_message_time(earliest_unread_timestamp);
        let rendered_instruction_prompt =
            self.render_instruction_prompt(unread_message_time.as_deref())?;
        let mut context = vec![ToolChatMessage::System {
            content: self.render_static_system_prompt(),
        }];
        // 原始图片只附加到配置窗口内的正文消息，更早的消息仍保留 Markdown 图片 ID。
        let vision_message_ids = Self::vision_message_ids(
            &message_ids,
            supports_vision,
            self.services.app_config.app.vision_image_message_window,
        );

        let mut unread_message_ids = Vec::new();
        context.push(ToolChatMessage::User {
            content: ToolChatUserContent::text("# 聊天记录\n"),
        });

        let mut last_rendered_date: Option<String> = None;
        let history_block_size = history_window.history_block_size;
        let mut last_history_block_index = None;
        let mut previous_message_id = None;
        let mut pending_block_boundary = false;
        let active_tool_histories = self.runtime_context.active_tool_histories().to_vec();
        for history in active_tool_histories
            .iter()
            .filter(|history| history.after_message_id.is_none())
        {
            history.extend_messages(&mut context);
        }

        for (history_index, db_msg) in history_window.messages.iter().enumerate() {
            let history_block_index = history_index / history_block_size;
            if last_history_block_index.is_some_and(|previous| previous != history_block_index) {
                pending_block_boundary = true;
            }
            if previous_message_id
                .is_some_and(|message_id| self.runtime_context.is_sealed_after(message_id))
            {
                pending_block_boundary = true;
            }
            last_history_block_index = Some(history_block_index);
            previous_message_id = Some(db_msg.id);

            let suppressed = active_tool_histories
                .iter()
                .any(|history| history.suppresses_message(db_msg.id));
            if !suppressed {
                let image_data_urls = if db_msg.sender_id != self.message_target.bot_id
                    && vision_message_ids.contains(&db_msg.id)
                {
                    self.load_message_image_data_urls(db_msg).await
                } else {
                    Vec::new()
                };
                Self::append_chat_message(
                    &mut context,
                    db_msg,
                    &self.message_target.bot_id,
                    pending_block_boundary,
                    &mut last_rendered_date,
                    &mut unread_message_ids,
                    image_data_urls,
                );
                pending_block_boundary = false;
            }

            for history in active_tool_histories
                .iter()
                .filter(|history| history.after_message_id == Some(db_msg.id))
            {
                history.extend_messages(&mut context);
            }
        }

        if let Some(prompt) = transient_user_prompt {
            context.push(ToolChatMessage::User {
                content: ToolChatUserContent::text(prompt),
            });
        }
        // 动态 instruction 放在初始上下文末尾，避免它的变化破坏前面固定内容的缓存。
        context.push(ToolChatMessage::System {
            content: rendered_instruction_prompt.content,
        });

        Ok(BuiltContext {
            messages: context,
            unread_message_ids,
            unread_message_time,
            pending_expired_character_memory_ids: rendered_instruction_prompt
                .pending_expired_character_memory_ids,
            latest_message_id: message_ids.last().copied(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn append_chat_message(
        context: &mut Vec<ToolChatMessage>,
        db_msg: &ChatMessage,
        bot_id: &str,
        starts_new_block: bool,
        last_rendered_date: &mut Option<String>,
        unread_message_ids: &mut Vec<i64>,
        image_data_urls: Vec<String>,
    ) {
        if db_msg.sender_id == bot_id {
            Self::append_assistant_history_message(context, db_msg, starts_new_block);
            return;
        }
        if !db_msg.is_read {
            unread_message_ids.push(db_msg.id);
        }

        let dt_utc = DateTime::<Utc>::from_timestamp(db_msg.event_timestamp, 0).unwrap();
        let dt_local: DateTime<Local> = DateTime::<Local>::from(dt_utc);
        let date_line = dt_local.format("%Y-%m-%d").to_string();
        let message_line = Self::history_message_line(db_msg, &dt_local);
        let should_render_date = last_rendered_date.as_deref() != Some(date_line.as_str());

        if let Some(ToolChatMessage::User { content }) = context.last_mut() {
            if !starts_new_block {
                if should_render_date {
                    content.push_text(&format!("\n{}", date_line));
                    *last_rendered_date = Some(date_line);
                }
                content.push_text(&format!("\n{}", message_line));
                for data_url in image_data_urls {
                    content.push_image(data_url);
                }
                return;
            }
        }

        let content = if should_render_date {
            *last_rendered_date = Some(date_line.clone());
            format!("{}\n{}", date_line, message_line)
        } else {
            message_line
        };
        let mut content = ToolChatUserContent::text(content);
        for data_url in image_data_urls {
            content.push_image(data_url);
        }
        context.push(ToolChatMessage::User { content });
    }

    fn append_assistant_history_message(
        context: &mut Vec<ToolChatMessage>,
        db_msg: &ChatMessage,
        starts_new_block: bool,
    ) {
        let message = db_msg.content_text.clone().unwrap_or_default();
        if !starts_new_block {
            if let Some(ToolChatMessage::Assistant {
                content: Some(content),
                tool_calls,
                ..
            }) = context.last_mut()
            {
                if tool_calls.is_empty() {
                    content.push('\n');
                    content.push_str(&message);
                    return;
                }
            }
        }
        context.push(ToolChatMessage::Assistant {
            content: Some(message),
            reasoning: None,
            tool_calls: Vec::new(),
        });
    }

    /// 从消息富文本片段读取图片，并生成本轮请求使用的临时 data URL。
    async fn load_message_image_data_urls(&self, db_msg: &ChatMessage) -> Vec<String> {
        let Ok(serde_json::Value::Array(parts)) =
            serde_json::from_str::<serde_json::Value>(&db_msg.content_parts_json)
        else {
            warn!(message_id = db_msg.id, "解析消息图片片段失败");
            return Vec::new();
        };

        let mut data_urls = Vec::new();
        for part in parts {
            if part.get("kind").and_then(serde_json::Value::as_str) != Some("image") {
                continue;
            }
            let Some(data) = part.get("data") else {
                continue;
            };
            let Some(local_path) = data.get("local_path").and_then(serde_json::Value::as_str)
            else {
                continue;
            };

            let result = async {
                let bytes = fs::read(local_path).await?;
                let mime_type = Self::image_mime_type_from_path(local_path);
                MessageEnricher::prepare_vision_image_data_url(&bytes, Some(mime_type))
            }
            .await;
            match result {
                Ok(data_url) => data_urls.push(data_url),
                Err(error) => warn!(
                    message_id = db_msg.id,
                    path = %local_path,
                    error = %error,
                    "读取聊天上下文图片失败，仅保留图片 ID"
                ),
            }
        }
        data_urls
    }

    fn image_mime_type_from_path(path: &str) -> &'static str {
        match Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("jpg") | Some("jpeg") => "image/jpeg",
            Some("png") => "image/png",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            Some("bmp") => "image/bmp",
            _ => "image/jpeg",
        }
    }

    /// 返回允许携带原始图片数据的消息 ID；窗口以正文消息条数计算。
    fn vision_message_ids(
        message_ids: &[i64],
        supports_vision: bool,
        message_window: usize,
    ) -> HashSet<i64> {
        if !supports_vision {
            return HashSet::new();
        }
        message_ids
            .iter()
            .rev()
            .take(message_window)
            .copied()
            .collect()
    }

    /// 渲染当前指令模板，替换日期、场景和本轮未读边界等动态内容。
    fn render_instruction_prompt(
        &self,
        unread_message_time: Option<&str>,
    ) -> anyhow::Result<RenderedPrompt> {
        let date_text = Local::now().format("%Y-%m-%d").to_string();
        let tasks = self
            .services
            .scheduler
            .running_tasks(&self.message_target)?;
        let task_summaries = tasks.iter().map(|task| task.summary()).collect::<Vec<_>>();
        let scheduled_tasks_json = serde_json::to_string_pretty(&task_summaries)?;
        let (character_memories, pending_expired_character_memory_ids) =
            self.character_memory.render_prompt()?;
        let recent_user_memories = self.user_memory.render_prompt()?;
        let scene = self.render_scene();
        let instruction_prompt = Self::replace_optional_prompt_line(
            &self.services.app_config.prompt_config.instruction_prompt,
            "{{unread_message_time}}",
            unread_message_time,
        );
        let content = instruction_prompt
            .replace("{{date}}", &date_text)
            .replace("{{scene}}", &scene)
            .replace("{{scheduled_tasks}}", &scheduled_tasks_json)
            .replace("{{character_memories}}", &character_memories)
            .replace("{{recent_user_memories}}", &recent_user_memories);
        Ok(RenderedPrompt {
            content,
            pending_expired_character_memory_ids,
        })
    }

    async fn ensure_group_info_loaded(&self) {
        if !matches!(
            self.message_target.conversation.kind,
            ConversationKind::Group
        ) {
            return;
        }
        self.group_info
            .get_or_init(|| async {
                match self
                    .services
                    .message_sender
                    .get_group_info(&self.message_target)
                    .await
                {
                    Ok(group_info) => group_info,
                    Err(error) => {
                        warn!(error = %error, "查询群聊基础资料失败，仅显示群聊场景");
                        None
                    }
                }
            })
            .await;
    }

    fn render_scene(&self) -> String {
        match self.group_info.get().and_then(Option::as_ref) {
            Some(group_info) => format!(
                "{}\n- 当前群名称:{}\n- 当前群成员数量:{}",
                self.scene, group_info.name, group_info.member_count
            ),
            None => self.scene.clone(),
        }
    }

    fn render_static_system_prompt(&self) -> String {
        format!(
            "{}\n\n{}",
            self.services.app_config.prompt_config.system_prompt,
            self.services.app_config.prompt_config.character_prompt,
        )
    }

    fn refresh_instruction_prompt(
        &self,
        messages: &mut [ToolChatMessage],
        unread_message_time: Option<&str>,
    ) -> anyhow::Result<Vec<i64>> {
        let instruction_prompt = self.render_instruction_prompt(unread_message_time)?;
        let Some(content) = messages.iter_mut().rev().find_map(|message| match message {
            ToolChatMessage::System { content } => Some(content),
            _ => None,
        }) else {
            anyhow::bail!("工具循环上下文缺少 instruction 提示词");
        };
        *content = instruction_prompt.content;
        Ok(instruction_prompt.pending_expired_character_memory_ids)
    }

    fn render_unread_message_time(earliest_unread_timestamp: Option<i64>) -> Option<String> {
        let timestamp = earliest_unread_timestamp?;
        let dt_utc = DateTime::<Utc>::from_timestamp(timestamp, 0)?;
        let dt_local: DateTime<Local> = DateTime::<Local>::from(dt_utc);
        Some(dt_local.format("%Y-%m-%d %H:%M").to_string())
    }

    /// 没有可用值时移除占位符所在行，避免向模型发送不完整的动态状态。
    fn replace_optional_prompt_line(
        prompt: &str,
        placeholder: &str,
        value: Option<&str>,
    ) -> String {
        match value {
            Some(value) => prompt.replace(placeholder, value),
            None => prompt
                .split_inclusive('\n')
                .filter(|line| !line.contains(placeholder))
                .collect(),
        }
    }

    /// 将数据库消息格式化为提示词里的聊天记录行。
    fn history_message_line(db_msg: &ChatMessage, dt_local: &DateTime<Local>) -> String {
        let time_text = dt_local.format("%H:%M").to_string();
        let sender_name = db_msg
            .sender_nickname
            .clone()
            .unwrap_or(db_msg.sender_display_name.clone());
        let content = db_msg.content_text.clone().unwrap_or_default();

        format!("{}（{}）:{}", sender_name, time_text, content)
    }
}
