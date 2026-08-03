use crate::ai_provider::{ContextMessage, MessageRole, ToolChatMessage, ToolChatResponse};
use chrono::{DateTime, Local, Utc};
use std::future::Future;
use std::sync::Arc;
use tokio::time::{timeout, Duration};

use crate::conversation_control::ConversationControl;
use crate::conversation_trigger::{ConversationTrigger, ConversationTriggerSender};
use crate::repository::db_manager::ChatMessage;
use crate::tools::{
    ConversationToolContext, ToolContext, ToolDefinition, ToolRegistry, ToolResult, ToolServices,
};
use crate::transport::message::{IncomingMessage, MessageTarget};
use crate::transport::SendOptions;

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
    messages: Vec<ContextMessage>,
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
        incoming_messages: Vec<IncomingMessage>,
        tools: &ToolRegistry,
    ) {
        // Actor 在过滤完成后立即进入这里，回复延时从此刻开始计算。
        let send_options = SendOptions::delay_started_now();
        for incoming_message in &incoming_messages {
            println!(
                "收到{}信息，会话 {}，{}: {:?}",
                self.scene,
                self.conversation_key,
                incoming_message.sender.display_name,
                incoming_message.content.text
            );
        }
        self.process_conversation(&incoming_messages, tools, None, send_options)
            .await;
    }

    /// 处理工具产生的内部触发；临时提示只参与本次请求，不写入数据库。
    pub async fn process_internal_trigger(
        &self,
        trigger: ConversationTrigger,
        tools: &ToolRegistry,
    ) {
        let send_options = SendOptions::delay_started_now();
        let ConversationTrigger {
            conversation_messages,
            user_prompt,
        } = trigger;
        println!(
            "收到内部触发，会话 {}：{}",
            self.conversation_key, user_prompt
        );
        self.process_conversation(
            &conversation_messages,
            tools,
            Some(&user_prompt),
            send_options,
        )
        .await;
    }

    async fn process_conversation(
        &self,
        conversation_messages: &[IncomingMessage],
        tools: &ToolRegistry,
        transient_user_prompt: Option<&str>,
        send_options: SendOptions,
    ) {
        let Some(conversation_snapshot) = conversation_messages.last() else {
            eprintln!("会话 {} 的触发缺少消息上下文", self.conversation_key);
            return;
        };
        let built_context = match self.build_context(conversation_snapshot) {
            Ok(context) => context,
            Err(err) => {
                eprintln!(
                    "构造会话 {} 的聊天上下文失败: {}",
                    self.conversation_key, err
                );
                return;
            }
        };

        let mut request_messages = built_context
            .messages
            .iter()
            .map(ToolChatMessage::from)
            .collect::<Vec<_>>();
        if let Some(prompt) = transient_user_prompt {
            request_messages.push(ToolChatMessage::User {
                content: prompt.to_string(),
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

        for tool_round in 0..=MAX_TOOL_ROUNDS {
            let response = match self.request_ai(&request_messages, &tool_definitions).await {
                Ok(response) => response,
                Err(err) => {
                    eprintln!("AI 请求最终失败: {}", err);
                    return;
                }
            };

            if !messages_marked_read {
                if let Err(err) = self
                    .services
                    .db_manager
                    .mark_messages_read(&built_context.unread_message_ids)
                {
                    eprintln!(
                        "更新会话 {} 的消息已读状态失败: {}",
                        self.conversation_key, err
                    );
                }
                messages_marked_read = true;
            }

            println!(
                "AI 原始返回（第 {} 次请求）：\n{}",
                tool_round + 1,
                serde_json::to_string_pretty(&response.raw_response)
                    .unwrap_or_else(|error| format!("序列化原始返回失败: {}", error))
            );

            if let Some(content) = Self::non_empty_response_content(response.content.as_deref()) {
                if let Err(err) = self
                    .services
                    .message_sender
                    .send_text(&self.message_target, content, send_options)
                    .await
                {
                    eprintln!("发送会话 {} 的 AI 回复失败: {}", self.conversation_key, err);
                }
            }

            if response.tool_calls.is_empty() {
                return;
            }

            if tool_round == MAX_TOOL_ROUNDS {
                eprintln!(
                    "会话 {} 连续工具调用达到 {} 轮，停止继续请求 AI",
                    self.conversation_key, MAX_TOOL_ROUNDS
                );
                return;
            }

            let assistant_message = response.assistant_message();
            let mut tool_results = Vec::new();
            for tool_call in response.tool_calls {
                println!(
                    "调用工具：{}，参数：{}",
                    tool_call.name, tool_call.arguments
                );
                let result = tools.execute(&tool_context, tool_call).await;
                println!(
                    "工具结果：{}，call_id={}，is_error={}，requires_ai_response={}，content={}",
                    result.tool_name,
                    result.tool_call_id,
                    result.is_error,
                    result.requires_ai_response,
                    if result.content.is_empty() {
                        "<无返回值>"
                    } else {
                        &result.content
                    }
                );
                tool_results.push(result);
            }

            if !Self::should_continue_tool_loop(&tool_results) {
                println!("本轮工具均不需要 AI 后续响应，结束工具循环");
                return;
            }

            request_messages.push(assistant_message);
            for result in &tool_results {
                request_messages.push(ToolChatMessage::from(result));
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
                    println!(
                        "AI 思考：{}",
                        resp.reasoning.display_text.as_deref().unwrap_or("")
                    );
                    println!("AI 回复：{}", resp.content.as_deref().unwrap_or(""));
                    println!("AI 工具调用数：{}", resp.tool_calls.len());
                    return Ok(resp);
                }
                Err(err) => {
                    eprintln!("AI 请求错误，第 {}/{} 次: {}", attempt, max_attempts, err);
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
    fn build_context(&self, incoming_message: &IncomingMessage) -> anyhow::Result<BuiltContext> {
        let mut context = Vec::new();
        let system_content = format!(
            "{}\n\n{}\n\n{}",
            self.services.app_config.prompt_config.system_prompt,
            self.services.app_config.prompt_config.character_prompt,
            self.render_instruction_prompt(),
        );
        context.push(ContextMessage {
            role: MessageRole::System,
            content: system_content,
        });

        let history: Vec<ChatMessage> = self.services.db_manager.get_conversation_history(
            &incoming_message.source,
            &incoming_message.conversation.id,
            self.services.app_config.app.max_history_messages,
        )?;
        let mut unread_message_ids = Vec::new();
        context.push(ContextMessage {
            role: MessageRole::User,
            content: "# 聊天记录\n".to_string(),
        });

        let mut last_rendered_date: Option<String> = None;
        let mut unread_divider_inserted = false;
        let history_block_size =
            Self::history_block_size(self.services.app_config.app.max_history_messages);
        let mut next_user_message_starts_new_block = false;
        for (history_index, db_msg) in history.into_iter().enumerate() {
            if history_index > 0 && history_index % history_block_size == 0 {
                next_user_message_starts_new_block = true;
            }

            let is_bot_message = db_msg.sender_id == incoming_message.bot_id.as_str();
            if !is_bot_message && !db_msg.is_read && !unread_divider_inserted {
                Self::push_unread_divider(&mut context);
                unread_divider_inserted = true;
            }
            if !is_bot_message && !db_msg.is_read {
                unread_message_ids.push(db_msg.id);
            }

            if is_bot_message {
                context.push(ContextMessage {
                    role: MessageRole::Assistant,
                    content: db_msg.content_text.unwrap_or_default(),
                });
            } else {
                let dt_utc = DateTime::<Utc>::from_timestamp(db_msg.event_timestamp, 0).unwrap();
                let dt_local: DateTime<Local> = DateTime::<Local>::from(dt_utc);
                let date_line = dt_local.format("%Y-%m-%d").to_string();
                let message_line = Self::history_message_line(&db_msg, &dt_local);
                let should_render_date = last_rendered_date.as_deref() != Some(date_line.as_str());

                if context
                    .last()
                    .is_some_and(|msg| msg.role == MessageRole::User)
                    && !next_user_message_starts_new_block
                {
                    let last_user_message = context.last_mut().unwrap();
                    if should_render_date {
                        last_user_message
                            .content
                            .push_str(&format!("\n{}", date_line));
                        last_rendered_date = Some(date_line);
                    }
                    last_user_message
                        .content
                        .push_str(&format!("\n{}", message_line));
                } else {
                    let content = if should_render_date {
                        last_rendered_date = Some(date_line.clone());
                        format!("{}\n{}", date_line, message_line)
                    } else {
                        message_line
                    };
                    context.push(ContextMessage {
                        role: MessageRole::User,
                        content,
                    });
                }
                next_user_message_starts_new_block = false;
            }
        }

        Ok(BuiltContext {
            messages: context,
            unread_message_ids,
        })
    }

    /// 在聊天记录里插入已读和未读消息的分界线。
    fn push_unread_divider(context: &mut Vec<ContextMessage>) {
        let divider = "--- 以下是未读消息 ---";

        if context
            .last()
            .is_some_and(|msg| msg.role == MessageRole::User)
        {
            context
                .last_mut()
                .unwrap()
                .content
                .push_str(&format!("\n{}", divider));
        } else {
            context.push(ContextMessage {
                role: MessageRole::User,
                content: divider.to_string(),
            });
        }
    }

    /// 聊天记录按和数据库淘汰逻辑一致的块大小拆分为多个 user 消息。
    fn history_block_size(max_history_messages: u32) -> usize {
        ((max_history_messages as usize) / 5).max(1)
    }

    /// 渲染当前指令模板，替换日期和场景占位符。
    fn render_instruction_prompt(&self) -> String {
        let date_text = Local::now().format("%Y-%m-%d").to_string();
        self.services
            .app_config
            .prompt_config
            .instruction_prompt
            .replace("{{date}}", &date_text)
            .replace("{{scene}}", &self.scene)
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
