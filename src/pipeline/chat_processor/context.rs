use super::{BuiltContext, ChatProcessor, RenderedPrompt};
use crate::ai_provider::{ToolChatMessage, ToolChatUserContent};
use crate::chat_history::{load_chat_history_context, render_history_message_line};
use crate::history_compression::render_summary_date_heading;
use crate::message_enricher::{read_image_bytes, MessageEnricher};
use crate::repository::db_manager::{ChatMessage, ConversationDailySummary};
use crate::tools::ToolRegistry;
use crate::transport::message::{ConversationKind, IncomingMessage};
use chrono::{DateTime, Local, Utc};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use tracing::{debug, warn};

impl ChatProcessor {
    /// 构建发送给聊天模型的完整上下文，包括系统提示词、聊天历史和当前指令。
    pub(super) async fn build_context(
        &mut self,
        current_messages: &[IncomingMessage],
        current_message_ids: &[i64],
        transient_user_prompt: Option<&str>,
        memory_review: bool,
    ) -> anyhow::Result<BuiltContext> {
        let memory_enabled =
            ToolRegistry::is_enabled(&self.services.app_config.app.enabled_actions, "memory");
        let group_memory_enabled = memory_enabled
            && matches!(
                self.message_target.conversation.kind,
                ConversationKind::Group
            );
        let deleted_memories = if group_memory_enabled {
            self.group_memory.begin_turn()?
        } else {
            0
        };
        if deleted_memories > 0 {
            debug!(memory_count = deleted_memories, "已删除确认遗忘的群记忆");
        }
        let supports_vision = self.services.app_config.chat_model_supports_vision();
        let history = load_chat_history_context(
            &self.services.db_manager,
            &self.message_target.source,
            &self.message_target.conversation.id,
            match self.message_target.conversation.kind {
                ConversationKind::Group => self.services.app_config.app.group_max_history_messages,
                ConversationKind::Direct => {
                    self.services.app_config.app.direct_max_history_messages
                }
            },
            self.services.app_config.app.history_summary_enabled,
            self.services.app_config.app.history_summary_days,
        )?;
        let history_window = history.window;
        let daily_summaries = history.summaries;
        let message_ids = history_window
            .messages
            .iter()
            .map(|message| message.id)
            .collect::<Vec<_>>();
        if self.runtime_context.reconcile_message_ids(&message_ids)? {
            debug!("聊天窗口已淘汰或删除旧记录，重新计算内存分块");
        }
        if memory_review {
            self.user_memory
                .refresh_history_users(&history_window.messages)
                .await;
        } else {
            self.user_memory
                .refresh_active_users(
                    &history_window.messages,
                    current_messages,
                    current_message_ids,
                )
                .await;
        }
        self.ensure_group_info_loaded().await;
        let earliest_unread_timestamp = history_window
            .messages
            .iter()
            .find(|message| message.sender_id != self.message_target.bot_id && !message.is_read)
            .map(|message| message.event_timestamp);
        let unread_message_time = Self::render_unread_message_time(earliest_unread_timestamp);
        let rendered_instruction_prompt =
            self.render_instruction_prompt(unread_message_time.as_deref())?;
        let group_conversation = matches!(
            self.message_target.conversation.kind,
            ConversationKind::Group
        );
        let system_prompt = crate::config::render_prompt_sections(
            &self.services.app_config.prompt_config.system_prompt,
            &[
                ("group_conversation", group_conversation),
                ("direct_conversation", !group_conversation),
            ],
        )?;
        let mut context = vec![
            ToolChatMessage::System {
                content: system_prompt,
            },
            ToolChatMessage::System {
                content: self
                    .services
                    .app_config
                    .prompt_config
                    .character_prompt
                    .clone(),
            },
            ToolChatMessage::System {
                content: self
                    .services
                    .app_config
                    .prompt_config
                    .reply_rules_prompt
                    .clone(),
            },
        ];
        // 原始图片只附加到配置窗口内的正文消息，更早的消息仍保留 Markdown 图片 ID。
        let vision_message_ids = Self::vision_message_ids(
            &message_ids,
            supports_vision,
            self.services.app_config.app.vision_image_message_window,
        );

        let mut unread_message_ids = Vec::new();
        Self::append_history_sections(&mut context, &daily_summaries)?;

        let mut last_rendered_date: Option<String> = None;
        let history_block_size = history_window.history_block_size;
        let mut last_history_block_index = None;
        let mut previous_message_id = None;
        let mut pending_block_boundary = false;
        let active_tool_histories = self.runtime_context.active_tool_histories().to_vec();
        let mut image_cache = HashMap::new();
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
                    self.load_message_image_data_urls(db_msg, &mut image_cache)
                        .await
                } else {
                    Vec::new()
                };
                if db_msg.sender_id != self.message_target.bot_id
                    || !self
                        .runtime_context
                        .append_assistant_response(db_msg.id, &mut context)
                {
                    Self::append_chat_message(
                        &mut context,
                        db_msg,
                        &self.message_target.bot_id,
                        pending_block_boundary,
                        &mut last_rendered_date,
                        &mut unread_message_ids,
                        image_data_urls,
                    );
                }
                pending_block_boundary = false;
            }

            for history in active_tool_histories
                .iter()
                .filter(|history| history.after_message_id == Some(db_msg.id))
            {
                history.extend_messages(&mut context);
            }
        }

        // 动态 instruction 放在聊天历史之后，临时任务说明紧随其后。
        let instruction_prompt_index = context.len();
        context.push(ToolChatMessage::System {
            content: rendered_instruction_prompt.content,
        });
        if let Some(prompt) = transient_user_prompt {
            context.push(ToolChatMessage::User {
                content: ToolChatUserContent::text(prompt),
            });
        }

        Ok(BuiltContext {
            messages: context,
            instruction_prompt_index,
            unread_message_ids,
            unread_message_time,
            pending_expired_group_memory_ids: rendered_instruction_prompt
                .pending_expired_group_memory_ids,
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
            Self::append_assistant_history_message(context, db_msg);
            return;
        }
        if !db_msg.is_read {
            unread_message_ids.push(db_msg.id);
        }

        let dt_utc = DateTime::<Utc>::from_timestamp(db_msg.event_timestamp, 0).unwrap();
        let dt_local: DateTime<Local> = DateTime::<Local>::from(dt_utc);
        let date_line = dt_local.format("%Y-%m-%d").to_string();
        let message_line = render_history_message_line(db_msg, &dt_local);
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

    fn append_assistant_history_message(context: &mut Vec<ToolChatMessage>, db_msg: &ChatMessage) {
        let message = db_msg.content_text.clone().unwrap_or_default();
        context.push(ToolChatMessage::Assistant {
            content: Some(message),
            reasoning: None,
            tool_calls: Vec::new(),
        });
    }

    /// 标题和全部摘要合并为一个消息块，原始消息使用固定窗口标题。
    fn append_history_sections(
        context: &mut Vec<ToolChatMessage>,
        summaries: &[ConversationDailySummary],
    ) -> anyhow::Result<()> {
        let mut history = String::from("# 聊天记录\n");
        if !summaries.is_empty() {
            history.push_str("\n已压缩的历史消息：");
            for summary in summaries {
                let heading = render_summary_date_heading(&summary.summary_date)?;
                history.push_str(&format!("\n\n{}\n{}", heading, summary.summary_text));
            }
            context.push(ToolChatMessage::User {
                content: ToolChatUserContent::text(history),
            });
            history = String::new();
        }
        history.push_str("\n当前聊天窗口消息：");
        context.push(ToolChatMessage::User {
            content: ToolChatUserContent::text(history),
        });
        Ok(())
    }

    /// 从消息富文本片段读取图片，并生成本轮请求使用的临时 data URL。
    async fn load_message_image_data_urls(
        &self,
        db_msg: &ChatMessage,
        cache: &mut HashMap<String, Option<String>>,
    ) -> Vec<String> {
        let parts = match serde_json::from_str::<serde_json::Value>(&db_msg.content_parts_json) {
            Ok(serde_json::Value::Array(parts)) => parts,
            Ok(_) => {
                warn!(message_id = db_msg.id, "消息图片片段不是 JSON 数组");
                return Vec::new();
            }
            Err(error) => {
                warn!(
                    message_id = db_msg.id,
                    error = %format!("{error:#}"),
                    "解析消息图片片段失败"
                );
                return Vec::new();
            }
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

            if let Some(cached) = cache.get(local_path) {
                if let Some(data_url) = cached {
                    data_urls.push(data_url.clone());
                }
                continue;
            }
            let result = async {
                let bytes = read_image_bytes(std::path::Path::new(local_path)).await?;
                let mime_type = MessageEnricher::mime_type_from_path(local_path);
                MessageEnricher::prepare_vision_image_data_url(&bytes, Some(mime_type))
            }
            .await;
            cache.insert(local_path.to_string(), result.as_ref().ok().cloned());
            match result {
                Ok(data_url) => data_urls.push(data_url),
                Err(error) => warn!(
                    message_id = db_msg.id,
                    path = %local_path,
                    error = %format!("{error:#}"),
                    "读取聊天上下文图片失败，仅保留图片 ID"
                ),
            }
        }
        data_urls
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
        let memory_enabled =
            ToolRegistry::is_enabled(&self.services.app_config.app.enabled_actions, "memory");
        let group_memory_enabled = memory_enabled
            && matches!(
                self.message_target.conversation.kind,
                ConversationKind::Group
            );
        let (group_memories, pending_expired_group_memory_ids) = if group_memory_enabled {
            self.group_memory.render_prompt()?
        } else {
            (String::new(), Vec::new())
        };
        let recent_user_memories = self.user_memory.render_prompt(memory_enabled)?;
        let scene = self.render_scene();
        let instruction_prompt = crate::config::render_prompt_sections(
            &self.services.app_config.prompt_config.instruction_prompt,
            &[
                ("unread", unread_message_time.is_some()),
                ("group_memory", group_memory_enabled),
            ],
        )?;
        let content = crate::config::render_prompt_template(
            &instruction_prompt,
            &[
                ("date", &date_text),
                ("scene", &scene),
                ("scheduled_tasks", &scheduled_tasks_json),
                ("group_memories", &group_memories),
                ("recent_user_memories", &recent_user_memories),
                ("unread_message_time", unread_message_time.unwrap_or("")),
            ],
        );
        Ok(RenderedPrompt {
            content,
            pending_expired_group_memory_ids,
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
                        warn!(error = %format!("{error:#}"), "查询群聊基础资料失败，仅显示群聊场景");
                        None
                    }
                }
            })
            .await;
    }

    fn render_scene(&self) -> String {
        if !matches!(
            self.message_target.conversation.kind,
            ConversationKind::Group
        ) {
            return self.scene.clone();
        }

        let mut scene = format!(
            "{}\n- 当前聊天群号：{}",
            self.scene, self.message_target.conversation.id
        );
        if let Some(group_info) = self.group_info.get().and_then(Option::as_ref) {
            scene.push_str(&format!(
                "\n- 当前群名称:{}\n- 当前群成员数量:{}",
                group_info.name, group_info.member_count
            ));
        }
        scene
    }

    /// 为同一来源、机器人与会话生成固定且不暴露原始会话 ID 的 OpenRouter 会话标识。
    pub(super) fn openrouter_session_id(&self) -> String {
        let conversation_kind = match self.message_target.conversation.kind {
            ConversationKind::Direct => "direct",
            ConversationKind::Group => "group",
        };
        let mut hasher = Sha256::new();
        for value in [
            self.message_target.source.as_str(),
            self.message_target.bot_id.as_str(),
            conversation_kind,
            self.message_target.conversation.id.as_str(),
        ] {
            hasher.update(value.as_bytes());
            hasher.update([0]);
        }
        format!("rolava:chat:{:x}", hasher.finalize())
    }

    pub(super) fn refresh_instruction_prompt(
        &self,
        messages: &mut [ToolChatMessage],
        instruction_prompt_index: usize,
        unread_message_time: Option<&str>,
    ) -> anyhow::Result<Vec<i64>> {
        let instruction_prompt = self.render_instruction_prompt(unread_message_time)?;
        let Some(ToolChatMessage::System { content }) = messages.get_mut(instruction_prompt_index)
        else {
            anyhow::bail!("工具循环上下文缺少 instruction 提示词");
        };
        *content = instruction_prompt.content;
        Ok(instruction_prompt.pending_expired_group_memory_ids)
    }

    fn render_unread_message_time(earliest_unread_timestamp: Option<i64>) -> Option<String> {
        let timestamp = earliest_unread_timestamp?;
        let dt_utc = DateTime::<Utc>::from_timestamp(timestamp, 0)?;
        let dt_local: DateTime<Local> = DateTime::<Local>::from(dt_utc);
        Some(dt_local.format("%Y-%m-%d %H:%M:%S").to_string())
    }
}
