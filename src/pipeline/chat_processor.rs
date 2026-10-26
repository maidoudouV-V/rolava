use crate::ai_provider::{
    run_ai_request_with_timeout, ToolChatMessage, ToolChatResponse,
};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::OnceCell;
use tracing::{debug, debug_span, error, info, trace, warn, Instrument};

use crate::conversation_context::{ActiveToolHistory, RuntimeContextState, ToolRoundHistory};
use crate::conversation_control::ConversationControl;
use crate::conversation_trigger::{ConversationTrigger, ConversationTriggerSender};
use crate::memory::{GroupMemorySession, UserMemorySession};
use crate::request_statistics::{with_request_purpose, RequestPurpose};
use crate::tools::{
    ConversationEffect, ConversationToolContext, ToolContext, ToolDefinition, ToolRegistry,
    ToolResult, ToolServices,
};
use crate::transport::message::{
    preferred_sender_name, readable_message_text, ConversationKind, IncomingMessage, MessageTarget,
};
use crate::transport::{GroupInfo, SendOptions};

use super::filter::FilteredMessage;

mod context;

const TOOL_ROUND_REMINDER_THRESHOLD: usize = 8;

/// 为单个会话构造上下文、请求 AI 并处理响应。
pub struct ChatProcessor {
    services: Arc<ToolServices>,
    conversation_key: String,
    scene: String,
    message_target: MessageTarget,
    conversation_control: Arc<ConversationControl>,
    trigger_sender: Arc<dyn ConversationTriggerSender>,
    group_memory: Arc<GroupMemorySession>,
    user_memory: Arc<UserMemorySession>,
    group_info: OnceCell<Option<GroupInfo>>,
    runtime_context: RuntimeContextState,
}

struct BuiltContext {
    messages: Vec<ToolChatMessage>,
    instruction_prompt_index: usize,
    unread_message_ids: Vec<i64>,
    unread_message_time: Option<String>,
    pending_expired_group_memory_ids: Vec<i64>,
    latest_message_id: Option<i64>,
}

struct RenderedPrompt {
    content: String,
    pending_expired_group_memory_ids: Vec<i64>,
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
        let group_memory = Arc::new(GroupMemorySession::new(
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
            group_memory,
            user_memory,
            group_info: OnceCell::new(),
            runtime_context: RuntimeContextState::default(),
        }
    }

    pub fn reset_conversation_state(&mut self) {
        self.user_memory.reset();
        self.runtime_context.reset();
    }

    pub fn end_conversation(&mut self) {
        self.conversation_control.set_ai_filter_bypassed(false);
        let removed = self.runtime_context.compact_finished_conversation();
        debug!(tool_history_count = removed, "对话结束，已清除内存工具历史");
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
        let messages = incoming_messages
            .iter()
            .map(|message| {
                format!(
                    "{}: {}",
                    preferred_sender_name(
                        &message.sender.display_name,
                        message.sender.nickname.as_deref(),
                    ),
                    readable_message_text(
                        &message.content.text,
                        message
                            .content
                            .parts
                            .iter()
                            .map(|part| (part.kind.as_str(), &part.data)),
                    ),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        info!(scene = %self.scene, messages = %messages, "开始处理平台消息");
        for incoming_message in &incoming_messages {
            debug!(
                scene = %self.scene,
                sender = %preferred_sender_name(
                    &incoming_message.sender.display_name,
                    incoming_message.sender.nickname.as_deref(),
                ),
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
            false,
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
        let ConversationTrigger {
            user_prompt,
            memory_review,
            condition,
        } = trigger;
        if let Some(condition) = condition {
            match condition() {
                Ok(true) => {}
                Ok(false) => {
                    debug!("内部触发条件已失效，跳过本次处理");
                    return;
                }
                Err(error) => {
                    error!(error = %format!("{error:#}"), "检查内部触发条件失败，跳过本次处理");
                    return;
                }
            }
        }
        if memory_review {
            self.process_memory_review(&user_prompt, tools).await;
            return;
        }
        info!("收到内部会话触发");
        trace!(prompt = %user_prompt, "内部会话触发完整提示");
        self.process_conversation(&[], tools, Some(&user_prompt), send_options, &[], false)
            .await;
    }

    pub async fn process_memory_review(&mut self, prompt: &str, tools: &ToolRegistry) {
        // 整理仍使用主 AI 流程，只提供记忆工具和结束工具。
        let tools = tools.select(&[
            "set_group_memory",
            "delete_group_memory",
            "set_user_memory",
            "delete_user_memory",
            "set_conversation_state",
            "finish_response",
        ]);
        if tools.get("set_user_memory").is_none() {
            return;
        }
        let bypassed = self.conversation_control.ai_filter_bypassed();
        // 整理使用独立用户列表，避免扩大普通聊天的最近活跃用户范围。
        let review_user_memory = Arc::new(UserMemorySession::new(
            self.message_target.clone(),
            self.services.app_config.as_ref(),
            self.services.db_manager.clone(),
        ));
        let regular_user_memory = std::mem::replace(&mut self.user_memory, review_user_memory);
        self.process_conversation(
            &[],
            &tools,
            Some(prompt),
            SendOptions::delay_started_now(),
            &[],
            true,
        )
        .await;
        self.user_memory = regular_user_memory;
        self.conversation_control.set_ai_filter_bypassed(bypassed);
    }

    async fn process_conversation(
        &mut self,
        conversation_messages: &[IncomingMessage],
        tools: &ToolRegistry,
        transient_user_prompt: Option<&str>,
        send_options: SendOptions,
        current_message_ids: &[i64],
        silent: bool,
    ) {
        let built_context = match self
            .build_context(
                conversation_messages,
                current_message_ids,
                transient_user_prompt,
                silent,
            )
            .await
        {
            Ok(context) => context,
            Err(err) => {
                error!(error = %format!("{err:#}"), "构造聊天上下文失败");
                return;
            }
        };

        let unread_message_time = built_context.unread_message_time.clone();
        let mut request_messages = built_context.messages;
        let mut pending_expired_group_memory_ids = built_context.pending_expired_group_memory_ids;
        let mut displayed_expired_group_memory_ids = HashSet::new();
        let tool_definitions = tools.definitions();
        let tool_context = ToolContext {
            conversation: ConversationToolContext {
                key: self.conversation_key.clone(),
                target: self.message_target.clone(),
                current_messages: conversation_messages.to_vec(),
                control: self.conversation_control.clone(),
                trigger_sender: self.trigger_sender.clone(),
                group_memory: self.group_memory.clone(),
                user_memory: self.user_memory.clone(),
            },
            services: self.services.clone(),
        };
        let mut messages_marked_read = false;
        let mut tool_round_history = Vec::new();
        let mut tool_round_message_ids = Vec::new();
        let mut conversation_effect = ConversationEffect::None;

        for tool_round in 0.. {
            let ai_span = debug_span!(
                "ai_request",
                round = tool_round + 1,
                message_count = request_messages.len(),
                tool_count = tool_definitions.len()
            );
            let response = match self
                .request_ai(&request_messages, &tool_definitions, silent)
                .instrument(ai_span)
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    error!(error = %format!("{err:#}"), "AI 请求最终失败");
                    break;
                }
            };
            displayed_expired_group_memory_ids
                .extend(pending_expired_group_memory_ids.iter().copied());

            if !messages_marked_read {
                self.runtime_context.seal_current_tail();
                if let Err(err) = self
                    .services
                    .db_manager
                    .mark_messages_read(&built_context.unread_message_ids)
                {
                    error!(error = %format!("{err:#}"), "更新消息已读状态失败");
                }
                messages_marked_read = true;
            }

            let visible_content = Self::non_empty_response_content(response.content.as_deref());
            let mut emitted_message_ids = Vec::new();
            if let Some(content) = visible_content.filter(|_| !silent) {
                match self
                    .services
                    .message_sender
                    .send_text(&self.message_target, content, send_options)
                    .await
                {
                    Ok(sent_messages) => emitted_message_ids
                        .extend(sent_messages.into_iter().map(|message| message.database_id)),
                    Err(err) => {
                        error!(error = %format!("{err:#}"), "发送 AI 回复失败")
                    }
                }
            }

            let assistant_message = response.assistant_message();
            let tool_calls = response.tool_calls;
            if tool_calls.is_empty() {
                self.runtime_context
                    .remember_assistant_response(assistant_message, emitted_message_ids);
                break;
            }

            tool_round_message_ids.extend(emitted_message_ids);

            let mut tool_results = Vec::new();
            for tool_call in tool_calls {
                let tool_arguments = tool_call.arguments.clone();
                debug!(
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
                let tool_span = debug_span!("tool_call", tool_name = %tool_call.name);
                let started_at = Instant::now();
                let result = tools
                    .execute(&tool_context, tool_call)
                    .instrument(tool_span)
                    .await;
                let elapsed_ms = started_at.elapsed().as_millis() as u64;
                if result.is_error {
                    warn!(
                        tool_name = %result.tool_name,
                        tool_call_id = %result.tool_call_id,
                        requires_ai_response = result.requires_ai_response,
                        arguments = %tool_arguments,
                        error = %result.content,
                        elapsed_ms,
                        "工具调用失败"
                    );
                } else {
                    info!(
                        tool_name = %result.tool_name,
                        elapsed_ms,
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
                Err(error) => error!(error = %format!("{error:#}"), "保存内存工具轮次失败"),
            }

            if !Self::should_continue_tool_loop(&tool_results) {
                debug!("本轮工具执行完成，结束工具循环");
                break;
            }

            if tool_results.iter().any(|result| {
                !result.is_error
                    && matches!(
                        result.tool_name.as_str(),
                        "set_group_memory"
                            | "delete_group_memory"
                            | "set_user_memory"
                            | "delete_user_memory"
                    )
            }) {
                match self.refresh_instruction_prompt(
                    &mut request_messages,
                    built_context.instruction_prompt_index,
                    unread_message_time.as_deref(),
                ) {
                    Ok(pending_ids) => {
                        pending_expired_group_memory_ids = pending_ids;
                    }
                    Err(error) => {
                        error!(error = %format!("{error:#}"), "刷新记忆提示词失败");
                    }
                }
            }

            request_messages.push(assistant_message);
            request_messages.extend(tool_result_messages);
            if tool_round + 1 == TOOL_ROUND_REMINDER_THRESHOLD {
                warn!(
                    tool_rounds = TOOL_ROUND_REMINDER_THRESHOLD,
                    "连续工具调用次数较多，已追加最终答复提示"
                );
                request_messages.push(ToolChatMessage::System {
                    content: "系统提示：当前工具调用次数过多，请尽快回复最终答案".to_string(),
                });
            }
        }

        if !displayed_expired_group_memory_ids.is_empty() {
            let displayed_ids = displayed_expired_group_memory_ids
                .into_iter()
                .collect::<Vec<_>>();
            match self.group_memory.finish_turn(&displayed_ids) {
                Ok(marked) if marked > 0 => {
                    debug!(memory_count = marked, "已确认展示本轮未续期的到期群记忆");
                }
                Ok(_) => {}
                Err(error) => error!(error = %format!("{error:#}"), "确认到期群记忆展示状态失败"),
            }
        }

        if silent {
            return;
        }
        if conversation_effect == ConversationEffect::End {
            self.end_conversation();
        } else if !tool_round_history.is_empty()
            && (matches!(
                self.message_target.conversation.kind,
                ConversationKind::Group
            ) || tool_round_history
                .iter()
                .any(ToolRoundHistory::has_gemini_response))
        {
            match ActiveToolHistory::new(
                built_context.latest_message_id,
                tool_round_history,
                tool_round_message_ids,
            ) {
                Ok(history) => {
                    if let Err(error) = self.runtime_context.push_tool_history(history) {
                        error!(error = %format!("{error:#}"), "追加内存工具历史失败");
                    }
                }
                Err(error) => error!(error = %format!("{error:#}"), "构造内存工具历史失败"),
            }
        }
    }

    /// finish_response 成功时结束循环，否则按工具结果决定是否继续请求 AI。
    fn should_continue_tool_loop(tool_results: &[ToolResult]) -> bool {
        if tool_results
            .iter()
            .any(|result| !result.is_error && result.tool_name == "finish_response")
        {
            return false;
        }
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
        memory_review: bool,
    ) -> anyhow::Result<ToolChatResponse> {
        let max_attempts = self.services.app_config.app.ai_request_max_attempts();
        let session_id = self.openrouter_session_id();
        let mut last_error = None;

        for attempt in 1..=max_attempts {
            let started_at = Instant::now();
            let chat_result = {
                let chat_provider = self
                    .services
                    .app_config
                    .ai_models
                    .get(&self.services.app_config.app.chat_model_name)
                    .expect("找不到聊天模型配置");
                run_ai_request_with_timeout(
                    self.services.app_config.app.ai_request_timeout_seconds,
                    "聊天模型 API 请求",
                    with_request_purpose(
                        if memory_review {
                            RequestPurpose::MemoryReview
                        } else {
                            RequestPurpose::Chat
                        },
                        chat_provider.chat_completions_with_session(
                            messages,
                            tool_definitions,
                            Some(&session_id),
                        ),
                    ),
                )
                .await
            };

            let elapsed_ms = started_at.elapsed().as_millis() as u64;
            match chat_result {
                Ok(resp) => {
                    debug!(
                        attempt,
                        elapsed_ms,
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
                    warn!(attempt, max_attempts, elapsed_ms, error = %format!("{err:#}"), "AI 请求失败，准备重试");
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.expect("AI 请求重试循环至少应执行一次"))
    }

}
