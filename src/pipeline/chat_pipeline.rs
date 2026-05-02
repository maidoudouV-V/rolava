use crate::ai_provider::{ContextMessage, MessageRole};
use chrono::{DateTime, Local, Utc};
use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{sleep, sleep_until, timeout, Duration, Instant};

use crate::config::AppConfig;
use crate::pipeline::ai_action::{RespAction, RespActionPlan};
use crate::repository::db_manager::{ChatMessage, QQChatContextManager};
use crate::transport::message::{ConversationKind, IncomingMessage};
use crate::transport::onebot::OneBotHttpServer;

/// 每个会话 worker 的消息队列容量。
const CONVERSATION_CHANNEL_CAPACITY: usize = 128;
/// 聚合连续消息时，每次等待新消息的秒数。
const MESSAGE_BATCH_WAIT_SECS: u64 = 3;
/// 单次最多聚合的消息数量。
const MESSAGE_BATCH_MAX_MESSAGES: usize = 5;
/// schedule_follow_up 允许的最短等待秒数。
const FOLLOW_UP_DELAY_MIN_SECS: u64 = 5;
/// schedule_follow_up 允许的最长等待秒数。
const FOLLOW_UP_DELAY_MAX_SECS: u64 = 600;

pub struct ChatPipeline {
    pub db_manager: Arc<QQChatContextManager>,
    transport_server: Arc<OneBotHttpServer>,
    app_config: Arc<AppConfig>,
    conversation_workers: HashMap<String, mpsc::Sender<IncomingMessage>>,
}

struct LastActionPlan {
    mind_state: String,
    action_names: String,
    updated_at_secs: i64,
}

struct ConversationWorker {
    db_manager: Arc<QQChatContextManager>,
    transport_server: Arc<OneBotHttpServer>,
    app_config: Arc<AppConfig>,
    conversation_key: String,
    scene: String,
    last_action_plan: Option<LastActionPlan>,
    pending_follow_up: Option<PendingFollowUp>,
}

/// 等待中的后续跟进任务。
struct PendingFollowUp {
    /// 到达这个时间且期间没有新消息时，触发后续跟进。
    ready_at: Instant,
    /// 本次后续跟进原计划等待的秒数。
    delay_seconds: u64,
    /// 安排后续跟进的原因，会写入下一次请求的触发原因。
    reason: String,
    /// 用来保留会话、平台和机器人信息的消息快照；不会作为新消息写入数据库。
    conversation_snapshot: IncomingMessage,
}

/// 一次发送给模型的请求消息列表，以及其中包含的数据库消息 ID。
struct BuiltChatContext {
    messages: Vec<ContextMessage>,
    included_message_ids: Vec<i64>,
}

impl ChatPipeline {
    /// 创建聊天处理流水线，主循环只负责接收消息并分发给会话 worker。
    pub fn new(db_manager: Arc<QQChatContextManager>, transport_server: Arc<OneBotHttpServer> , app_config: AppConfig) -> Self {
        Self {
            db_manager,
            transport_server,
            app_config: Arc::new(app_config),
            conversation_workers: HashMap::new(),
        }
    }

    /// 持续接收新消息，过滤机器人自身消息和白名单之外的消息后按会话分发。
    pub async fn start_process_chat(&mut self) {
        loop {
            let incoming_message = self.transport_server.recv_latest_message().await;
            if self.should_skip_message(&incoming_message) {
                continue;
            }

            let conversation_key = Self::conversation_key(&incoming_message);
            let worker_tx = self.get_or_spawn_worker(&conversation_key, &incoming_message);
            if worker_tx.send(incoming_message).await.is_err() {
                eprintln!("会话 worker 已关闭，消息分发失败: {}", conversation_key);
                self.conversation_workers.remove(&conversation_key);
            }
        }
    }

    /// 获取已有会话 worker；不存在时创建一个新的会话 worker。
    fn get_or_spawn_worker(&mut self, conversation_key: &str, incoming_message: &IncomingMessage) -> mpsc::Sender<IncomingMessage> {
        if let Some(worker_tx) = self.conversation_workers.get(conversation_key) {
            return worker_tx.clone();
        }

        let (worker_tx, worker_rx) = mpsc::channel(CONVERSATION_CHANNEL_CAPACITY);
        let worker = ConversationWorker::new(
            self.db_manager.clone(),
            self.transport_server.clone(),
            self.app_config.clone(),
            conversation_key.to_string(),
            Self::scene_name(&incoming_message.conversation.kind).to_string(),
        );
        tokio::spawn(worker.run(worker_rx));
        self.conversation_workers.insert(conversation_key.to_string(), worker_tx.clone());
        worker_tx
    }

    /// 判断消息是否应该跳过，包括机器人自身消息、白名单之外消息和暂不支持的频道消息。
    fn should_skip_message(&self, incoming_message: &IncomingMessage) -> bool {
        if incoming_message.sender.id == incoming_message.bot_id {
            return true;
        }
        if !self.is_message_allowed(incoming_message) {
            println!(
                "跳过非白名单消息 {}:{} sender={}",
                incoming_message.source,
                incoming_message.conversation.id,
                incoming_message.sender.id
            );
            return true;
        }
        false
    }

    /// 判断消息是否通过私聊或群聊白名单；白名单为空时放行对应类型的所有消息。
    fn is_message_allowed(&self, incoming_message: &IncomingMessage) -> bool {
        match incoming_message.conversation.kind {
            ConversationKind::Direct => {
                self.app_config.app.direct_whitelist.is_empty()
                    || self.app_config.app.direct_whitelist.contains(&incoming_message.sender.id)
            }
            ConversationKind::Group => {
                self.app_config.app.group_whitelist.is_empty()
                    || self.app_config.app.group_whitelist.contains(&incoming_message.conversation.id)
            }
            ConversationKind::Channel => false,
        }
    }

    /// 生成会话 worker key，避免不同平台、会话类型或会话 ID 之间互相污染状态。
    fn conversation_key(incoming_message: &IncomingMessage) -> String {
        let kind = match incoming_message.conversation.kind {
            ConversationKind::Direct => "direct",
            ConversationKind::Group => "group",
            ConversationKind::Channel => "channel",
        };
        format!("{}:{}:{}", incoming_message.source, kind, incoming_message.conversation.id)
    }

    /// 将会话类型转换成提示词中的当前对话场景名称。
    fn scene_name(kind: &ConversationKind) -> &'static str {
        match kind {
            ConversationKind::Direct => "私聊",
            ConversationKind::Group => "群聊",
            ConversationKind::Channel => "频道",
        }
    }
}

impl ConversationWorker {
    /// 创建一个只处理单个会话消息的 worker。
    fn new(
        db_manager: Arc<QQChatContextManager>,
        transport_server: Arc<OneBotHttpServer>,
        app_config: Arc<AppConfig>,
        conversation_key: String,
        scene: String,
    ) -> Self {
        Self {
            db_manager,
            transport_server,
            app_config,
            conversation_key,
            scene,
            last_action_plan: None,
            pending_follow_up: None,
        }
    }

    /// 按顺序处理当前会话的消息；私聊和群聊都会先做短时间消息聚合。
    async fn run(mut self, mut message_rx: mpsc::Receiver<IncomingMessage>) {
        while let Some(first_message) = self.recv_message_or_follow_up(&mut message_rx).await {
            let incoming_messages = match first_message.conversation.kind {
                ConversationKind::Direct | ConversationKind::Group => {
                    Self::collect_conversation_messages(first_message, &mut message_rx).await
                }
                ConversationKind::Channel => continue,
            };
            self.process_chat_messages(incoming_messages).await;
        }
    }

    /// 等待下一条消息或已安排的后续跟进；新消息会取消尚未触发的后续跟进。
    async fn recv_message_or_follow_up(
        &mut self,
        message_rx: &mut mpsc::Receiver<IncomingMessage>,
    ) -> Option<IncomingMessage> {
        loop {
            let Some(ready_at) = self.pending_follow_up.as_ref().map(|follow_up| follow_up.ready_at) else {
                return message_rx.recv().await;
            };

            tokio::select! {
                biased;
                incoming_message = message_rx.recv() => {
                    if incoming_message.is_some() {
                        self.pending_follow_up = None;
                    }
                    return incoming_message;
                }
                _ = sleep_until(ready_at) => {
                    if let Some(follow_up) = self.pending_follow_up.take() {
                        self.process_follow_up(follow_up).await;
                    }
                }
            }
        }
    }

    /// 聚合同一会话里的连续消息：2 秒内有新消息则重置等待，累计 5 条立即结束。
    async fn collect_conversation_messages(
        first_message: IncomingMessage,
        message_rx: &mut mpsc::Receiver<IncomingMessage>,
    ) -> Vec<IncomingMessage> {
        let mut messages = vec![first_message];
        while messages.len() < MESSAGE_BATCH_MAX_MESSAGES {
            match timeout(Duration::from_secs(MESSAGE_BATCH_WAIT_SECS), message_rx.recv()).await {
                Ok(Some(next_message)) => messages.push(next_message),
                Ok(None) | Err(_) => break,
            }
        }
        messages
    }

    /// 处理一批同一会话消息：写入历史、构建上下文、调用模型并执行动作。
    async fn process_chat_messages(&mut self, mut incoming_messages: Vec<IncomingMessage>) {
        incoming_messages.sort_by(|left, right| {
            left.timestamp
                .cmp(&right.timestamp)
                .then_with(|| left.message_id.cmp(&right.message_id))
        });
        let incoming_message = incoming_messages
            .last()
            .expect("incoming messages must not be empty")
            .clone();
        println!(
            "收到{}信息 {} 条，会话 {}，最新 {}: {:?}",
            self.scene,
            incoming_messages.len(),
            self.conversation_key,
            incoming_message.sender.display_name,
            incoming_message.content.text
        );

        for message in &incoming_messages {
            self.db_manager.write_incoming_message(message).unwrap();
        }
        let context = self.build_context(&incoming_message, "你收到了一条或多条新的聊天消息。");
        let recent_messages_start = context.messages.len().saturating_sub(5);
        println!(
            "构建上下文：\n {}",
            serde_json::to_string_pretty(&context.messages[recent_messages_start..]).unwrap()
        );

        if let Some(action_plan) = self.request_ai_action_plan(&context.messages).await {
            self.handle_action_plan(
                &incoming_message,
                action_plan,
                &context.included_message_ids,
            ).await;
        }
    }

    /// 处理已到期的后续跟进：不写入新消息，只重新读取当前会话历史并唤醒模型。
    async fn process_follow_up(&mut self, follow_up: PendingFollowUp) {
        let trigger_reason = format!(
            "已等待 {} 秒无人继续发言。等待原因：{}",
            follow_up.delay_seconds,
            follow_up.reason
        );
        println!(
            "触发后续跟进，会话 {}，原因: {}",
            self.conversation_key,
            follow_up.reason
        );

        let context = self.build_context(&follow_up.conversation_snapshot, &trigger_reason);
        println!("构建后续跟进上下文：\n {}", serde_json::to_string_pretty(&context.messages).unwrap());
        if let Some(action_plan) = self.request_ai_action_plan(&context.messages).await {
            self.handle_action_plan(
                &follow_up.conversation_snapshot,
                action_plan,
                &context.included_message_ids,
            ).await;
        }
    }

    /// 请求聊天模型并解析为动作计划；解析失败会按当前策略重试一次。
    async fn request_ai_action_plan(&self, messages: &Vec<ContextMessage>) -> Option<RespActionPlan> {
        for _ in 0..2 {
            let chat_result = {
                let chat_provider = self
                    .app_config
                    .ai_providers
                    .get(&self.app_config.app.chat_model_name)
                    .expect("Chat model not found");
                chat_provider.chat_completions(messages).await
            };

            match chat_result {
                Ok(resp) => {
                    println!("AI 思考：{}", resp.reasoning_content.as_ref().unwrap_or(&"".to_string()));
                    println!("AI 回复：{}", resp.content);
                    if let Ok(action_plan) = serde_json::from_str::<RespActionPlan>(resp.content.as_str()) {
                        return Some(action_plan);
                    } else {
                        eprintln!("AI 回复格式错误，无法解析为 RespActionPlan: {}", resp.content);
                    }
                }
                Err(e) => {
                    eprintln!("AI 请求错误: {}", e);
                }
            }
        }
        None
    }

    /// 处理已经成功解析的动作计划：标记已读、执行动作，并更新下一轮可继承的最近状态。
    async fn handle_action_plan(
        &mut self,
        incoming_message: &IncomingMessage,
        action_plan: RespActionPlan,
        included_message_ids: &[i64],
    ) {
        if let Err(err) = self.db_manager.mark_messages_read(included_message_ids) {
            eprintln!("标记聊天记录消息为已读失败: {}", err);
        }
        let RespActionPlan { mind_state, actions } = action_plan;
        let action_names = Self::action_names(&actions);
        self.execute_actions(incoming_message, actions).await;
        self.last_action_plan = Some(LastActionPlan {
            mind_state,
            action_names,
            updated_at_secs: Utc::now().timestamp(),
        });
    }

    /// 构建发送给聊天模型的完整上下文，包括系统提示词、聊天历史和当前指令。
    fn build_context(&self, incoming_message: &IncomingMessage, trigger_reason: &str) -> BuiltChatContext {
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

        let history: Vec<ChatMessage> = self
            .db_manager
            .get_conversation_history(
                &incoming_message.source,
                &incoming_message.conversation.id,
                self.app_config.app.max_history_messages,
            )
            .unwrap_or_default();
        let mut included_message_ids = Vec::with_capacity(history.len());

        context.push(ContextMessage {
            role: MessageRole::User,
            content: "# 聊天记录\n".to_string(),
        });

        let mut last_rendered_date: Option<String> = None;
        let mut has_read_user_messages = false;
        let mut unread_divider_inserted = false;
        for db_msg in history {
            included_message_ids.push(db_msg.id);
            let is_bot_message = db_msg.sender_id == incoming_message.bot_id.as_str();
            if !is_bot_message && !db_msg.is_read && !unread_divider_inserted {
                if has_read_user_messages {
                    Self::push_unread_divider(&mut context);
                }
                unread_divider_inserted = true;
            }
            if !is_bot_message && db_msg.is_read {
                has_read_user_messages = true;
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

                if context.last().is_some_and(|msg| msg.role == MessageRole::User) {
                    let last_user_message = context.last_mut().unwrap();
                    if should_render_date {
                        last_user_message.content.push_str(&format!("\n{}", date_line));
                        last_rendered_date = Some(date_line);
                    }
                    last_user_message.content.push_str(&format!("\n{}", message_line));
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
            }
        }

        context.push(ContextMessage {
            role: MessageRole::User,
            content: self.render_instruction_prompt(trigger_reason),
        });

        BuiltChatContext {
            messages: context,
            included_message_ids,
        }
    }

    /// 在聊天记录里插入已读和未读消息的分界线。
    fn push_unread_divider(context: &mut Vec<ContextMessage>) {
        let divider = "--- 以上是已读消息，以下是未读消息 ---";

        if context.last().is_some_and(|msg| msg.role == MessageRole::User) {
            context.last_mut().unwrap().content.push_str(&format!("\n{}", divider));
        } else {
            context.push(ContextMessage {
                role: MessageRole::User,
                content: divider.to_string(),
            });
        }
    }

    /// 渲染当前指令模板，替换时间、场景、随机数和上一轮动作状态等动态占位符。
    fn render_instruction_prompt(&self, trigger_reason: &str) -> String {
        let now_secs = Utc::now().timestamp();
        let now_text = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let random_0_99 = rand::thread_rng().gen_range(0..100).to_string();
        let (last_action, mind_state) = if let Some(last_action_plan) = &self.last_action_plan {
            let mind_state = if now_secs - last_action_plan.updated_at_secs > 24 * 60 * 60 {
                "距离上次对话已超过24小时，已经遗忘。".to_string()
            } else {
                last_action_plan.mind_state.clone()
            };
            (last_action_plan.action_names.clone(), mind_state)
        } else {
            ("无动作".to_string(), "暂无".to_string())
        };

        self.app_config.prompt_config.instruction_prompt
            .replace("{{now}}", &now_text)
            .replace("{{last_action}}", &last_action)
            .replace("{{mind_state}}", &mind_state)
            .replace("{{scene}}", &self.scene)
            .replace("{{random_0_99}}", &random_0_99)
            .replace("{{max_history_messages}}", &self.app_config.app.max_history_messages.to_string())
            .replace("{{trigger_reason}}", trigger_reason)
    }

    /// 按顺序执行模型返回的动作，每个动作由自己的方法处理副作用。
    async fn execute_actions(&mut self, incoming_message: &IncomingMessage, actions: Vec<RespAction>) {
        let mut next_message_ready_at_secs = incoming_message.timestamp as f64;
        for action in actions {
            match action {
                RespAction::SendMessage { text } => {
                    next_message_ready_at_secs = self.execute_send_message(
                        incoming_message,
                        text,
                        next_message_ready_at_secs,
                    ).await;
                }
                RespAction::CallTool { tool, args } => {
                    self.execute_call_tool(tool, args).await;
                }
                RespAction::Remember { content } => {
                    self.execute_remember(content).await;
                }
                RespAction::ScheduleFollowUp { delay_seconds, reason } => {
                    self.execute_schedule_follow_up(incoming_message, delay_seconds, reason);
                }
            }
        }
    }

    /// 执行发送消息动作，并返回下一条消息延迟计算的起点时间。
    async fn execute_send_message(
        &self,
        incoming_message: &IncomingMessage,
        text: String,
        mut next_message_ready_at_secs: f64,
    ) -> f64 {
        let reply_char_count = text.chars().count() as f64;
        let base_delay_secs = reply_char_count * self.app_config.app.reply_delay_per_char_secs;
        let random_extra_delay_secs = if self.app_config.app.reply_delay_random_max_secs > 0.0 {
            rand::thread_rng().gen_range(0.0..=self.app_config.app.reply_delay_random_max_secs)
        } else {
            0.0
        };

        next_message_ready_at_secs += base_delay_secs + random_extra_delay_secs;
        let now_secs = Utc::now().timestamp_millis() as f64 / 1000.0;
        if now_secs < next_message_ready_at_secs {
            sleep(Duration::from_secs_f64(next_message_ready_at_secs - now_secs)).await;
        }

        self.transport_server
            .send_message(incoming_message.clone(), &text, self.db_manager.clone())
            .await
            .expect("Failed to send reply");
        Utc::now().timestamp_millis() as f64 / 1000.0
    }

    /// 执行工具调用动作；当前只记录日志，后续接入真实工具实现。
    async fn execute_call_tool(&self, tool: String, args: serde_json::Value) {
        eprintln!("暂未实现工具调用: tool={}, args={}", tool, args);
    }

    /// 执行记忆写入动作；当前只记录日志，后续接入真实记忆库。
    async fn execute_remember(&self, content: String) {
        eprintln!("暂未实现记忆写入: content={}", content);
    }

    /// 执行后续跟进动作，直接更新当前 worker 的待触发跟进状态。
    fn execute_schedule_follow_up(
        &mut self,
        incoming_message: &IncomingMessage,
        delay_seconds: u64,
        reason: String,
    ) {
        let delay_seconds = delay_seconds.clamp(FOLLOW_UP_DELAY_MIN_SECS, FOLLOW_UP_DELAY_MAX_SECS);
        eprintln!(
            "安排后续跟进: delay_seconds={}, reason={}",
            delay_seconds,
            reason
        );
        self.pending_follow_up = Some(PendingFollowUp {
            ready_at: Instant::now() + Duration::from_secs(delay_seconds),
            delay_seconds,
            reason,
            conversation_snapshot: incoming_message.clone(),
        });
    }

    /// 将本轮动作列表压缩成动作类型数组字符串，用于下一轮提示词中的上一轮动作。
    fn action_names(actions: &[RespAction]) -> String {
        if actions.is_empty() {
            "无动作".to_string()
        } else {
            let names: Vec<&str> = actions.iter()
                .map(RespAction::action_name)
                .collect();
            format!("[{}]", names.join(", "))
        }
    }

    /// 将数据库消息格式化为提示词里的聊天记录行。
    fn history_message_line(db_msg: &ChatMessage, dt_local: &DateTime<Local>) -> String {
        let time_text = dt_local.format("%H:%M").to_string();
        let sender_name = db_msg.sender_nickname
            .clone()
            .unwrap_or(db_msg.sender_display_name.clone());
        let content = db_msg.content_text.clone().unwrap_or_default();

        format!("{}（{}）:{}", sender_name, time_text, content)
    }
}
