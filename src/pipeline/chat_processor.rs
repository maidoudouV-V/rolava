use crate::ai_provider::{ContextMessage, MessageRole, ToolChatMessage, ToolChatResponse};
use chrono::{DateTime, Local, Utc};
use std::future::Future;
use std::sync::Arc;
use tokio::time::{timeout, Duration};

use crate::config::AppConfig;
use crate::repository::db_manager::{ChatMessage, QQChatContextManager};
use crate::tools::{ToolContext, ToolRegistry};
use crate::transport::message::{IncomingMessage, MessageTarget};
use crate::transport::MessageSender;

/// 为单个会话构造上下文、请求 AI 并处理响应。
pub struct ChatProcessor {
    pub db_manager: Arc<QQChatContextManager>,
    app_config: Arc<AppConfig>,
    conversation_key: String,
    scene: String,
    message_sender: Arc<dyn MessageSender>,
    message_target: MessageTarget,
}

struct BuiltContext {
    messages: Vec<ContextMessage>,
    unread_message_ids: Vec<i64>,
}

impl ChatProcessor {
    /// 创建一个只服务于指定会话的聊天处理器。
    pub fn new(
        db_manager: Arc<QQChatContextManager>,
        app_config: Arc<AppConfig>,
        conversation_key: String,
        scene: String,
        message_sender: Arc<dyn MessageSender>,
        message_target: MessageTarget,
    ) -> Self {
        Self {
            db_manager,
            app_config,
            conversation_key,
            scene,
            message_sender,
            message_target,
        }
    }

    /// 处理平台发来的消息。过滤和入库已由当前会话 Actor 在调用前完成。
    pub async fn process_messages(
        &self,
        incoming_messages: Vec<IncomingMessage>,
        tools: &ToolRegistry,
    ) {
        for incoming_message in &incoming_messages {
            println!(
                "收到{}信息，会话 {}，{}: {:?}",
                self.scene,
                self.conversation_key,
                incoming_message.sender.display_name,
                incoming_message.content.text
            );
        }
        self.process_conversation(&incoming_messages, tools).await;
    }

    /// 处理延时工具等内部来源的触发，不重复执行平台消息过滤和入库。
    pub async fn process_internal_trigger(
        &self,
        conversation_messages: Vec<IncomingMessage>,
        tools: &ToolRegistry,
    ) {
        println!("收到内部触发，会话 {}", self.conversation_key);
        self.process_conversation(&conversation_messages, tools)
            .await;
    }

    async fn process_conversation(
        &self,
        conversation_messages: &[IncomingMessage],
        tools: &ToolRegistry,
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

        match self.request_ai(&built_context.messages, tools).await {
            Ok(response) => {
                if let Err(err) = self
                    .db_manager
                    .mark_messages_read(&built_context.unread_message_ids)
                {
                    eprintln!(
                        "更新会话 {} 的消息已读状态失败: {}",
                        self.conversation_key, err
                    );
                }

                println!(
                    "AI 原始返回：\n{}",
                    serde_json::to_string_pretty(&response.raw_response)
                        .unwrap_or_else(|error| format!("序列化原始返回失败: {}", error))
                );

                if let Some(content) = response
                    .content
                    .as_deref()
                    .filter(|content| !content.trim().is_empty())
                {
                    if let Err(err) = self
                        .message_sender
                        .send_text(&self.message_target, content)
                        .await
                    {
                        eprintln!("发送会话 {} 的 AI 回复失败: {}", self.conversation_key, err);
                    }
                }

                let tool_context = ToolContext {
                    conversation_key: self.conversation_key.clone(),
                    message_sender: self.message_sender.clone(),
                    message_target: self.message_target.clone(),
                };
                for tool_call in response.tool_calls {
                    println!(
                        "调用工具：{}，参数：{}",
                        tool_call.name, tool_call.arguments
                    );
                    let result = tools.execute(&tool_context, tool_call).await;
                    println!(
                        "工具结果：{}，call_id={}，is_error={}，content={}",
                        result.tool_name, result.tool_call_id, result.is_error, result.content
                    );
                }
            }
            Err(err) => eprintln!("AI 请求最终失败: {}", err),
        }
    }

    /// 使用已经构造好的上下文请求聊天模型，并返回通用 tools 响应。
    async fn request_ai(
        &self,
        messages: &[ContextMessage],
        tools: &ToolRegistry,
    ) -> anyhow::Result<ToolChatResponse> {
        let tool_messages = messages
            .iter()
            .map(ToolChatMessage::from)
            .collect::<Vec<_>>();
        let tool_definitions = tools.definitions();
        let max_attempts = self.app_config.app.ai_request_max_attempts();
        let mut last_error = None;

        for attempt in 1..=max_attempts {
            let chat_result = {
                let chat_provider = self
                    .app_config
                    .ai_models
                    .get(&self.app_config.app.chat_model_name)
                    .expect("找不到聊天模型配置");
                Self::run_ai_request_with_timeout(
                    self.app_config.app.ai_request_timeout_seconds,
                    "聊天模型 API 请求",
                    chat_provider.chat_completions(&tool_messages, &tool_definitions),
                )
                .await
            };

            match chat_result {
                Ok(resp) => {
                    println!(
                        "AI 思考：{}",
                        resp.reasoning_content.as_deref().unwrap_or("")
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
            "{}\n\n{}",
            self.app_config.prompt_config.system_prompt,
            self.app_config.prompt_config.character_prompt,
        );
        context.push(ContextMessage {
            role: MessageRole::System,
            content: system_content,
        });

        let history: Vec<ChatMessage> = self.db_manager.get_conversation_history(
            &incoming_message.source,
            &incoming_message.conversation.id,
            self.app_config.app.max_history_messages,
        )?;
        let mut unread_message_ids = Vec::new();
        context.push(ContextMessage {
            role: MessageRole::User,
            content: "# 聊天记录\n".to_string(),
        });

        let mut last_rendered_date: Option<String> = None;
        let mut unread_divider_inserted = false;
        let history_block_size = Self::history_block_size(self.app_config.app.max_history_messages);
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

        context.push(ContextMessage {
            role: MessageRole::User,
            content: self.render_instruction_prompt(),
        });

        Ok(BuiltContext {
            messages: context,
            unread_message_ids,
        })
    }

    /// 在聊天记录里插入已读和未读消息的分界线。
    fn push_unread_divider(context: &mut Vec<ContextMessage>) {
        let divider = "--- 以上是已读消息，以下是未读消息 ---";

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

    /// 渲染当前指令模板，替换时间和场景等动态占位符。
    fn render_instruction_prompt(&self) -> String {
        let now_text = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        self.app_config
            .prompt_config
            .instruction_prompt
            .replace("{{now}}", &now_text)
            .replace("{{scene}}", &self.scene)
            .replace(
                "{{max_history_messages}}",
                &self.app_config.app.max_history_messages.to_string(),
            )
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
