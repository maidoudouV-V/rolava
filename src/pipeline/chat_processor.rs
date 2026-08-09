use crate::ai_provider::{ToolChatMessage, ToolChatResponse, ToolChatUserContent};
use chrono::{DateTime, Local, Utc};
use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use tokio::fs;
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info, info_span, warn, Instrument};

use crate::conversation_context::{AiTurnBlock, AiTurnRound};
use crate::conversation_control::ConversationControl;
use crate::conversation_trigger::{ConversationTrigger, ConversationTriggerSender};
use crate::message_enricher::MessageEnricher;
use crate::repository::db_manager::{ChatMessage, ContextTimelineItem};
use crate::tools::{
    ConversationToolContext, ToolContext, ToolDefinition, ToolRegistry, ToolResult, ToolServices,
};
use crate::transport::message::{IncomingMessage, MessageTarget};
use crate::transport::SendOptions;

use super::filter::FilteredMessage;

const MAX_TOOL_ROUNDS: usize = 8;

/// 为单个会话构造上下文、请求 AI 并处理响应。
pub struct ChatProcessor {
    services: Arc<ToolServices>,
    conversation_key: String,
    scene: String,
    message_target: MessageTarget,
    conversation_control: Arc<ConversationControl>,
    trigger_sender: Arc<dyn ConversationTriggerSender>,
}

struct BuiltContext {
    messages: Vec<ToolChatMessage>,
    unread_message_ids: Vec<i64>,
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
        Self {
            services,
            conversation_key,
            scene,
            message_target,
            conversation_control,
            trigger_sender,
        }
    }

    /// 处理平台发来的消息。过滤和入库已由当前会话 Actor 在调用前完成。
    pub async fn process_messages(
        &self,
        filtered_messages: Vec<FilteredMessage>,
        tools: &ToolRegistry,
    ) {
        // Actor 在过滤完成后立即进入这里，回复延时从此刻开始计算。
        let send_options = SendOptions::delay_started_now();
        let trigger_message_id = filtered_messages.first().map(|message| message.database_id);
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
            trigger_message_id,
        )
        .await;
    }

    /// 处理工具产生的内部触发；临时提示只参与本次请求，不写入数据库。
    pub async fn process_internal_trigger(
        &self,
        trigger: ConversationTrigger,
        tools: &ToolRegistry,
    ) {
        let send_options = SendOptions::delay_started_now();
        let ConversationTrigger { user_prompt } = trigger;
        info!("收到内部会话触发");
        debug!(prompt = %user_prompt, "内部会话触发提示");
        let trigger_message_id = self
            .services
            .db_manager
            .get_latest_conversation_message_id(
                &self.message_target.source,
                &self.message_target.conversation.id,
            )
            .map(|message_id| (message_id > 0).then_some(message_id))
            .unwrap_or_else(|error| {
                warn!(error = %error, "读取内部触发依赖的正文消息失败");
                None
            });
        self.process_conversation(
            &[],
            tools,
            Some(&user_prompt),
            send_options,
            trigger_message_id,
        )
        .await;
    }

    async fn process_conversation(
        &self,
        conversation_messages: &[IncomingMessage],
        tools: &ToolRegistry,
        transient_user_prompt: Option<&str>,
        send_options: SendOptions,
        trigger_message_id: Option<i64>,
    ) {
        let built_context = match self.build_context().await {
            Ok(context) => context,
            Err(err) => {
                error!(error = %err, "构造聊天上下文失败");
                return;
            }
        };

        let mut request_messages = built_context.messages;
        if let Some(prompt) = transient_user_prompt {
            request_messages.push(ToolChatMessage::User {
                content: ToolChatUserContent::text(prompt),
            });
        }
        let tool_definitions = tools.definitions();
        let tool_context = ToolContext {
            conversation: ConversationToolContext {
                key: self.conversation_key.clone(),
                target: self.message_target.clone(),
                current_messages: conversation_messages.to_vec(),
                control: self.conversation_control.clone(),
                trigger_sender: self.trigger_sender.clone(),
            },
            services: self.services.clone(),
        };
        let mut messages_marked_read = false;
        let mut ai_turn = AiTurnBlock::default();
        let mut first_emitted_message_id = None;

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

            if !messages_marked_read {
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
            if let Some(content) = visible_content {
                match self
                    .services
                    .message_sender
                    .send_text(&self.message_target, content, send_options)
                    .await
                {
                    Ok(sent_messages) => {
                        if first_emitted_message_id.is_none() {
                            first_emitted_message_id =
                                sent_messages.first().map(|message| message.database_id);
                        }
                    }
                    Err(err) => {
                        error!(error = %err, "发送 AI 回复失败")
                    }
                }
            }

            let assistant_message = response.assistant_message();
            let tool_calls = response.tool_calls;
            if tool_calls.is_empty() {
                // 初次请求的合法空 stop 是无动作；工具链后的空 stop 则用于完整关闭该链。
                if visible_content.is_some() || !ai_turn.rounds.is_empty() {
                    ai_turn
                        .rounds
                        .push(AiTurnRound::for_history(assistant_message, Vec::new()));
                }
                break;
            }

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
                ai_turn
                    .rounds
                    .push(AiTurnRound::for_history(assistant_message, tool_results));
                break;
            }

            let mut tool_results = Vec::new();
            for tool_call in tool_calls {
                info!(
                    tool_name = %tool_call.name,
                    tool_call_id = %tool_call.id,
                    "开始调用工具"
                );
                debug!(
                    tool_name = %tool_call.name,
                    tool_call_id = %tool_call.id,
                    arguments = %tool_call.arguments,
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

            let tool_result_messages = tool_results
                .iter()
                .map(ToolChatMessage::from)
                .collect::<Vec<_>>();
            ai_turn.rounds.push(AiTurnRound::for_history(
                assistant_message.clone(),
                tool_result_messages.clone(),
            ));

            if !Self::should_continue_tool_loop(&tool_results) {
                debug!("本轮工具均不需要 AI 后续响应，结束工具循环");
                break;
            }

            request_messages.push(assistant_message);
            request_messages.extend(tool_result_messages);
        }

        if !ai_turn.rounds.is_empty() {
            match serde_json::to_string(&ai_turn) {
                Ok(payload_json) => {
                    let retention_message_id = first_emitted_message_id.or(trigger_message_id);
                    if let Some(retention_message_id) = retention_message_id {
                        if let Err(error) = self.services.db_manager.insert_ai_turn_context_block(
                            &self.message_target.source,
                            &self.message_target.conversation.id,
                            &payload_json,
                            retention_message_id,
                        ) {
                            error!(error = %error, "持久化 AI 上下文原子块失败");
                        }
                    } else {
                        warn!("AI 上下文原子块没有可用的正文锚点，本轮不持久化");
                    }
                }
                Err(error) => error!(error = %error, "序列化 AI 上下文原子块失败"),
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
                    debug!(
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
    async fn build_context(&self) -> anyhow::Result<BuiltContext> {
        let supports_vision = self.services.app_config.chat_model_supports_vision();
        let system_prompt = &self.services.app_config.prompt_config.system_prompt;
        let system_content = format!(
            "{}\n\n{}\n\n{}",
            system_prompt,
            self.services.app_config.prompt_config.character_prompt,
            self.render_instruction_prompt()?,
        );
        let mut context = vec![ToolChatMessage::System {
            content: system_content,
        }];

        let context_window = self.services.db_manager.get_conversation_context_window(
            &self.message_target.source,
            &self.message_target.conversation.id,
            self.services.app_config.app.max_history_messages,
        )?;
        if context_window.pruned_block_count > 0 {
            debug!(
                block_count = context_window.pruned_block_count,
                "已清理滚出正文窗口的上下文块"
            );
        }
        // 原始图片只附加到配置窗口内的正文消息，更早的消息仍保留 Markdown 图片 ID。
        let vision_message_ids = Self::vision_message_ids(
            &context_window.message_ids,
            supports_vision,
            self.services.app_config.app.vision_image_message_window,
        );

        let mut unread_message_ids = Vec::new();
        context.push(ToolChatMessage::User {
            content: ToolChatUserContent::text("# 聊天记录\n"),
        });

        let mut last_rendered_date: Option<String> = None;
        let mut unread_divider_inserted = false;
        let history_block_size =
            Self::history_block_size(self.services.app_config.app.max_history_messages);
        for item in context_window.items {
            match item {
                ContextTimelineItem::InputMessage {
                    history_index,
                    message: db_msg,
                    ..
                } => {
                    let starts_new_user_block =
                        history_index > 0 && history_index % history_block_size == 0;
                    let image_data_urls = if vision_message_ids.contains(&db_msg.id) {
                        self.load_message_image_data_urls(&db_msg).await
                    } else {
                        Vec::new()
                    };
                    Self::append_chat_message(
                        &mut context,
                        &db_msg,
                        starts_new_user_block,
                        &mut last_rendered_date,
                        &mut unread_divider_inserted,
                        &mut unread_message_ids,
                        image_data_urls,
                    );
                }
                ContextTimelineItem::AiTurn(ai_turn) => {
                    ai_turn.extend_messages(&mut context);
                }
            }
        }

        Ok(BuiltContext {
            messages: context,
            unread_message_ids,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn append_chat_message(
        context: &mut Vec<ToolChatMessage>,
        db_msg: &ChatMessage,
        starts_new_user_block: bool,
        last_rendered_date: &mut Option<String>,
        unread_divider_inserted: &mut bool,
        unread_message_ids: &mut Vec<i64>,
        image_data_urls: Vec<String>,
    ) {
        if !db_msg.is_read && !*unread_divider_inserted {
            Self::push_unread_divider(context);
            *unread_divider_inserted = true;
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
            if !starts_new_user_block {
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

    /// 在聊天记录里插入已读和未读消息的分界线。
    fn push_unread_divider(context: &mut Vec<ToolChatMessage>) {
        let divider = "--- 以下是未读消息 ---";

        if let Some(ToolChatMessage::User { content }) = context.last_mut() {
            content.push_text(&format!("\n{}", divider));
        } else {
            context.push(ToolChatMessage::User {
                content: ToolChatUserContent::text(divider),
            });
        }
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

    /// 聊天记录按和数据库淘汰逻辑一致的块大小拆分为多个 user 消息。
    fn history_block_size(max_history_messages: u32) -> usize {
        ((max_history_messages as usize) / 5).max(1)
    }

    /// 渲染当前指令模板，替换日期和场景占位符。
    fn render_instruction_prompt(&self) -> anyhow::Result<String> {
        let date_text = Local::now().format("%Y-%m-%d").to_string();
        let tasks = self
            .services
            .scheduler
            .running_tasks(&self.message_target)?;
        let task_summaries = tasks.iter().map(|task| task.summary()).collect::<Vec<_>>();
        let scheduled_tasks_json = serde_json::to_string_pretty(&task_summaries)?;
        Ok(self
            .services
            .app_config
            .prompt_config
            .instruction_prompt
            .replace("{{date}}", &date_text)
            .replace("{{scene}}", &self.scene)
            .replace("{{scheduled_tasks}}", &scheduled_tasks_json))
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

#[cfg(test)]
mod tests {
    use super::ChatProcessor;
    use crate::tools::ToolResult;

    #[test]
    fn tool_loop_stops_when_results_do_not_require_ai_response() {
        let results = vec![tool_result("任务添加成功", false), tool_result("", false)];

        assert!(!ChatProcessor::should_continue_tool_loop(&results));
    }

    #[test]
    fn tool_loop_continues_when_any_result_requires_ai_response() {
        let results = vec![tool_result("", true), tool_result("任务添加成功", false)];

        assert!(ChatProcessor::should_continue_tool_loop(&results));
    }

    #[test]
    fn empty_response_content_is_not_sent() {
        assert_eq!(ChatProcessor::non_empty_response_content(None), None);
        assert_eq!(ChatProcessor::non_empty_response_content(Some("")), None);
        assert_eq!(
            ChatProcessor::non_empty_response_content(Some("  \n")),
            None
        );
        assert_eq!(
            ChatProcessor::non_empty_response_content(Some("正常回复")),
            Some("正常回复")
        );
    }

    #[test]
    fn vision_images_follow_configured_message_window() {
        let message_ids = (1..=12).collect::<Vec<_>>();

        let visible_ids = ChatProcessor::vision_message_ids(&message_ids, true, 4);

        assert_eq!(visible_ids.len(), 4);
        assert!(!visible_ids.contains(&8));
        assert!(visible_ids.contains(&9));
        assert!(visible_ids.contains(&12));
        assert!(ChatProcessor::vision_message_ids(&message_ids, false, 4).is_empty());
        assert!(ChatProcessor::vision_message_ids(&message_ids, true, 0).is_empty());
    }

    fn tool_result(content: &str, requires_ai_response: bool) -> ToolResult {
        ToolResult {
            tool_call_id: "call_1".to_string(),
            tool_name: "test_tool".to_string(),
            content: content.to_string(),
            requires_ai_response,
            is_error: false,
        }
    }
}
