use crate::ai_provider::{ContextMessage, MessageRole};
use chrono::{DateTime, Local, NaiveDate, NaiveTime, TimeZone, Utc};
use rand::Rng;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{sleep, sleep_until, timeout, Duration, Instant};

use crate::config::AppConfig;
use crate::message_enricher::MessageEnricher;
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
/// wait_then_check 允许的最短等待秒数。
const WAIT_THEN_CHECK_DELAY_MIN_SECS: u64 = 5;
/// wait_then_check 允许的最长等待秒数。
const WAIT_THEN_CHECK_DELAY_MAX_SECS: u64 = 600;
/// ignore_messages 允许的最短忽略秒数。
const IGNORE_MESSAGES_MIN_SECS: u64 = 10;
/// ignore_messages 允许的最长忽略秒数。
const IGNORE_MESSAGES_MAX_SECS: u64 = 600;

pub struct ChatPipeline {
    pub db_manager: Arc<QQChatContextManager>,
    transport_server: Arc<OneBotHttpServer>,
    app_config: Arc<AppConfig>,
    message_enricher: MessageEnricher,
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
    pending_wait_check: Option<PendingWaitCheck>,
    pending_ignore_messages: Option<PendingIgnoreMessages>,
    pending_tool_actions: Option<PendingToolActions>,
    pending_scheduled_tasks: Vec<PendingScheduledTask>,
}

/// 等待中的重新查看任务。
struct PendingWaitCheck {
    /// 到达这个时间且期间没有新消息时，触发重新查看。
    ready_at: Instant,
    /// 本次重新查看原计划等待的秒数。
    delay_seconds: u64,
    /// 安排重新查看的原因，会写入下一次请求的触发原因。
    reason: String,
    /// 用来保留会话、平台和机器人信息的消息快照；不会作为新消息写入数据库。
    conversation_snapshot: IncomingMessage,
}

/// 等待中的绝对时间定时任务。
struct PendingScheduledTask {
    /// 任务到达这个时间时触发重新请求。
    ready_at: Instant,
    /// 模型传入的本地日期时间文本。
    scheduled_time_text: String,
    /// 任务描述，会写入下一次请求的触发原因。
    task: String,
    /// 用来保留会话、平台和机器人信息的消息快照；不会作为新消息写入数据库。
    conversation_snapshot: IncomingMessage,
}

/// 暂时忽略消息任务。
struct PendingIgnoreMessages {
    /// 到达这个时间后，结束忽略状态。
    ready_at: Instant,
    /// 本次忽略原计划持续的秒数。
    duration_seconds: u64,
    /// 忽略期间是否成功写入过新消息；只有有新未读消息时才唤醒模型。
    has_unread_messages: bool,
    /// 用来保留会话、平台和机器人信息的消息快照；不会作为新消息写入数据库。
    conversation_snapshot: IncomingMessage,
}

/// 等待中的工具动作组。
struct PendingToolActions {
    /// 本轮所有工具动作的聚合任务。
    handle: JoinHandle<Vec<ToolActionResult>>,
    /// 工具执行期间是否写入过新消息；用于触发原因提示聊天模型。
    has_unread_messages: bool,
    /// 用来保留会话、平台和机器人信息的消息快照；不会作为新消息写入数据库。
    conversation_snapshot: IncomingMessage,
}

/// 单个工具动作的执行任务。
struct ToolActionTask {
    action_name: String,
    input_summary: String,
    handle: JoinHandle<anyhow::Result<String>>,
}

/// 单个工具动作的执行结果，会插入下一轮提示词。
struct ToolActionResult {
    action_name: String,
    input_summary: String,
    output: String,
    is_error: bool,
}

/// 一次发送给模型的请求消息列表，以及其中包含的数据库消息 ID。
struct BuiltChatContext {
    messages: Vec<ContextMessage>,
    included_message_ids: Vec<i64>,
}

impl ChatPipeline {
    /// 创建聊天处理流水线，主循环只负责接收消息并分发给会话 worker。
    pub fn new(
        db_manager: Arc<QQChatContextManager>,
        transport_server: Arc<OneBotHttpServer>,
        app_config: AppConfig,
    ) -> Self {
        let app_config = Arc::new(app_config);
        Self {
            db_manager: db_manager.clone(),
            transport_server,
            message_enricher: MessageEnricher::new(app_config.clone(), db_manager),
            app_config,
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
            let incoming_message = self.message_enricher.enrich(incoming_message).await;

            let conversation_key = Self::conversation_key(&incoming_message);
            let worker_tx = self.get_or_spawn_worker(&conversation_key, &incoming_message);
            if worker_tx.send(incoming_message).await.is_err() {
                eprintln!("会话 worker 已关闭，消息分发失败: {}", conversation_key);
                self.conversation_workers.remove(&conversation_key);
            }
        }
    }

    /// 获取已有会话 worker；不存在时创建一个新的会话 worker。
    fn get_or_spawn_worker(
        &mut self,
        conversation_key: &str,
        incoming_message: &IncomingMessage,
    ) -> mpsc::Sender<IncomingMessage> {
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
        self.conversation_workers
            .insert(conversation_key.to_string(), worker_tx.clone());
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
        format!(
            "{}:{}:{}",
            incoming_message.source, kind, incoming_message.conversation.id
        )
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
            pending_wait_check: None,
            pending_ignore_messages: None,
            pending_tool_actions: None,
            pending_scheduled_tasks: Vec::new(),
        }
    }

    /// 按顺序处理当前会话的消息；私聊和群聊都会先做短时间消息聚合。
    async fn run(mut self, mut message_rx: mpsc::Receiver<IncomingMessage>) {
        while let Some(first_message) = self.recv_message_or_wait_check(&mut message_rx).await {
            let incoming_messages = match first_message.conversation.kind {
                ConversationKind::Direct | ConversationKind::Group => {
                    Self::collect_conversation_messages(first_message, &mut message_rx).await
                }
                ConversationKind::Channel => continue,
            };
            self.process_chat_messages(incoming_messages).await;
        }
    }

    /// 等待下一条消息或已安排的重新查看；新消息会取消尚未触发的重新查看。
    async fn recv_message_or_wait_check(
        &mut self,
        message_rx: &mut mpsc::Receiver<IncomingMessage>,
    ) -> Option<IncomingMessage> {
        loop {
            if let Some(mut tool_actions) = self.pending_tool_actions.take() {
                tokio::select! {
                    incoming_message = message_rx.recv() => {
                        let Some(incoming_message) = incoming_message else {
                            return None;
                        };
                        if self.write_deferred_message(&incoming_message, "工具执行期间") {
                            tool_actions.has_unread_messages = true;
                        }
                        self.pending_tool_actions = Some(tool_actions);
                        continue;
                    }
                    result = &mut tool_actions.handle => {
                        self.process_tool_actions_completed(tool_actions, result).await;
                        continue;
                    }
                }
            }

            if let Some(ignore_ready_at) = self
                .pending_ignore_messages
                .as_ref()
                .map(|ignore| ignore.ready_at)
            {
                if Instant::now() < ignore_ready_at {
                    tokio::select! {
                        incoming_message = message_rx.recv() => {
                            let Some(incoming_message) = incoming_message else {
                                return None;
                            };
                            self.pending_wait_check = None;
                            if Self::mentions_bot(&incoming_message) {
                                self.pending_ignore_messages = None;
                                return Some(incoming_message);
                            }
                            if self.write_deferred_message(&incoming_message, "忽略期间") {
                                if let Some(ignore_messages) = self.pending_ignore_messages.as_mut() {
                                    ignore_messages.has_unread_messages = true;
                                }
                            }
                            continue;
                        }
                        _ = sleep_until(ignore_ready_at) => {
                            if let Some(ignore_messages) = self.pending_ignore_messages.take() {
                                self.process_ignore_messages_expired(ignore_messages).await;
                            }
                            continue;
                        }
                    }
                }
                if let Some(ignore_messages) = self.pending_ignore_messages.take() {
                    self.process_ignore_messages_expired(ignore_messages).await;
                    continue;
                }
            }

            if let Some(scheduled_task) = self.pop_due_scheduled_task() {
                self.process_scheduled_task(scheduled_task).await;
                continue;
            }

            let wait_check_ready_at = self
                .pending_wait_check
                .as_ref()
                .map(|wait_check| wait_check.ready_at);
            let scheduled_task_ready_at = self.next_scheduled_task_ready_at();
            let Some(ready_at) =
                Self::earliest_ready_at(wait_check_ready_at, scheduled_task_ready_at)
            else {
                return message_rx.recv().await;
            };

            tokio::select! {
                biased;
                incoming_message = message_rx.recv() => {
                    if incoming_message.is_some() {
                        self.pending_wait_check = None;
                    }
                    return incoming_message;
                }
                _ = sleep_until(ready_at) => {
                    if let Some(scheduled_task) = self.pop_due_scheduled_task() {
                        self.process_scheduled_task(scheduled_task).await;
                        continue;
                    }
                    if self.pending_wait_check.as_ref().is_some_and(|wait_check| wait_check.ready_at <= Instant::now()) {
                        let wait_check = self.pending_wait_check.take().unwrap();
                        self.process_wait_check(wait_check).await;
                    }
                }
            }
        }
    }

    fn earliest_ready_at(left: Option<Instant>, right: Option<Instant>) -> Option<Instant> {
        match (left, right) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(left), None) => Some(left),
            (None, Some(right)) => Some(right),
            (None, None) => None,
        }
    }

    fn next_scheduled_task_ready_at(&self) -> Option<Instant> {
        self.pending_scheduled_tasks
            .iter()
            .map(|task| task.ready_at)
            .min()
    }

    fn pop_due_scheduled_task(&mut self) -> Option<PendingScheduledTask> {
        let now = Instant::now();
        let task_index = self
            .pending_scheduled_tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| task.ready_at <= now)
            .min_by_key(|(_, task)| task.ready_at)
            .map(|(index, _)| index)?;
        Some(self.pending_scheduled_tasks.remove(task_index))
    }

    /// 判断消息是否 @ 了当前机器人账号。
    fn mentions_bot(incoming_message: &IncomingMessage) -> bool {
        incoming_message.content.parts.iter().any(|part| {
            part.kind == "at"
                && part
                    .data
                    .get("qq")
                    .and_then(|value| {
                        value
                            .as_str()
                            .map(ToString::to_string)
                            .or_else(|| value.as_i64().map(|number| number.to_string()))
                            .or_else(|| value.as_u64().map(|number| number.to_string()))
                    })
                    .is_some_and(|qq| qq == incoming_message.bot_id)
        })
    }

    /// 延迟请求期间的新消息只入库，不进入聚合和 AI 请求流程。
    fn write_deferred_message(&self, incoming_message: &IncomingMessage, reason: &str) -> bool {
        println!(
            "{}延迟处理{}消息，会话 {}，{}: {:?}",
            reason,
            self.scene,
            self.conversation_key,
            incoming_message.sender.display_name,
            incoming_message.content.text
        );
        if let Err(err) = self.db_manager.write_incoming_message(incoming_message) {
            eprintln!("写入延迟处理消息失败: {}", err);
            return false;
        }
        true
    }

    /// 聚合同一会话里的连续消息：2 秒内有新消息则重置等待，累计 5 条立即结束。
    async fn collect_conversation_messages(
        first_message: IncomingMessage,
        message_rx: &mut mpsc::Receiver<IncomingMessage>,
    ) -> Vec<IncomingMessage> {
        let mut messages = vec![first_message];
        while messages.len() < MESSAGE_BATCH_MAX_MESSAGES {
            match timeout(
                Duration::from_secs(MESSAGE_BATCH_WAIT_SECS),
                message_rx.recv(),
            )
            .await
            {
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
            .expect("待处理消息列表不应为空")
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
        let context =
            self.build_context(&incoming_message, "你收到了一条或多条新的聊天消息。", None);
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
            )
            .await;
        }
    }

    /// 处理已到期的重新查看：不写入新消息，只重新读取当前会话历史并唤醒模型。
    async fn process_wait_check(&mut self, wait_check: PendingWaitCheck) {
        let trigger_reason = format!(
            "已等待 {} 秒无人继续发言。等待原因：{}",
            wait_check.delay_seconds, wait_check.reason
        );
        println!(
            "触发重新查看，会话 {}，原因: {}",
            self.conversation_key, wait_check.reason
        );

        let context = self.build_context(&wait_check.conversation_snapshot, &trigger_reason, None);
        println!(
            "构建重新查看上下文：\n {}",
            serde_json::to_string_pretty(&context.messages).unwrap()
        );
        if let Some(action_plan) = self.request_ai_action_plan(&context.messages).await {
            self.handle_action_plan(
                &wait_check.conversation_snapshot,
                action_plan,
                &context.included_message_ids,
            )
            .await;
        }
    }

    /// 处理到期的定时任务：不写入新消息，只重新读取当前会话历史并唤醒模型。
    async fn process_scheduled_task(&mut self, scheduled_task: PendingScheduledTask) {
        let trigger_reason = format!(
            "定时任务已到时间。计划时间：{}。任务：{}",
            scheduled_task.scheduled_time_text, scheduled_task.task
        );
        println!(
            "触发定时任务，会话 {}，time={}，task={}",
            self.conversation_key, scheduled_task.scheduled_time_text, scheduled_task.task
        );

        let context =
            self.build_context(&scheduled_task.conversation_snapshot, &trigger_reason, None);
        println!(
            "构建定时任务上下文：\n {}",
            serde_json::to_string_pretty(&context.messages.last().unwrap()).unwrap()
        );
        if let Some(action_plan) = self.request_ai_action_plan(&context.messages).await {
            self.handle_action_plan(
                &scheduled_task.conversation_snapshot,
                action_plan,
                &context.included_message_ids,
            )
            .await;
        }
    }

    /// 处理已到期的忽略消息任务：如果期间有新未读消息，则主动请求模型重新决策。
    async fn process_ignore_messages_expired(&mut self, ignore_messages: PendingIgnoreMessages) {
        if !ignore_messages.has_unread_messages {
            println!(
                "结束忽略消息，会话 {}，期间没有新消息",
                self.conversation_key
            );
            return;
        }

        let trigger_reason = format!(
            "暂时忽略已结束，过去 {} 秒内收到新的未读消息，请重新查看聊天记录并决定下一步动作。",
            ignore_messages.duration_seconds
        );
        println!("结束忽略消息并触发重新查看，会话 {}", self.conversation_key);

        let context = self.build_context(
            &ignore_messages.conversation_snapshot,
            &trigger_reason,
            None,
        );
        println!(
            "构建忽略结束上下文：\n {}",
            serde_json::to_string_pretty(&context.messages).unwrap()
        );
        if let Some(action_plan) = self.request_ai_action_plan(&context.messages).await {
            self.handle_action_plan(
                &ignore_messages.conversation_snapshot,
                action_plan,
                &context.included_message_ids,
            )
            .await;
        }
    }

    /// 处理工具动作组完成：把工具结果插入下一轮提示词，并重新请求聊天模型。
    async fn process_tool_actions_completed(
        &mut self,
        tool_actions: PendingToolActions,
        task_result: Result<Vec<ToolActionResult>, tokio::task::JoinError>,
    ) {
        let tool_results = match task_result {
            Ok(results) => results,
            Err(err) => vec![ToolActionResult {
                action_name: "tool_actions".to_string(),
                input_summary: "本轮工具动作组".to_string(),
                output: format!("工具动作组异常：{}", err),
                is_error: true,
            }],
        };
        let trigger_reason = if tool_actions.has_unread_messages {
            "本轮工具动作已全部完成，且工具执行期间收到了新的聊天消息。请结合工具结果和新的聊天记录重新决定下一步动作。".to_string()
        } else {
            "本轮工具动作已全部完成。请结合工具结果重新决定下一步动作。".to_string()
        };
        println!(
            "工具动作组完成，会话 {}，tool_count={}，has_new_messages={}",
            self.conversation_key,
            tool_results.len(),
            tool_actions.has_unread_messages
        );

        let context = self.build_context(
            &tool_actions.conversation_snapshot,
            &trigger_reason,
            Some(&tool_results),
        );
        println!(
            "构建工具动作完成上下文：\n {}",
            serde_json::to_string_pretty(&context.messages.last().unwrap()).unwrap()
        );
        if let Some(action_plan) = self.request_ai_action_plan(&context.messages).await {
            self.handle_action_plan(
                &tool_actions.conversation_snapshot,
                action_plan,
                &context.included_message_ids,
            )
            .await;
        }
    }

    /// 请求聊天模型并解析为动作计划；解析失败会按当前策略重试一次。
    async fn request_ai_action_plan(
        &self,
        messages: &Vec<ContextMessage>,
    ) -> Option<RespActionPlan> {
        let max_attempts = self.app_config.app.ai_request_max_attempts();
        for attempt in 1..=max_attempts {
            let chat_result = {
                let chat_provider = self
                    .app_config
                    .ai_providers
                    .get(&self.app_config.app.chat_model_name)
                    .expect("找不到聊天模型配置");
                Self::run_ai_request_with_timeout(
                    self.app_config.app.ai_request_timeout_seconds,
                    "聊天模型 API 请求",
                    chat_provider.chat_completions(messages),
                )
                .await
            };

            match chat_result {
                Ok(resp) => {
                    println!(
                        "AI 思考：{}",
                        resp.reasoning_content.as_ref().unwrap_or(&"".to_string())
                    );
                    println!("AI 回复：{}", resp.content);
                    if let Ok(action_plan) =
                        serde_json::from_str::<RespActionPlan>(resp.content.as_str())
                    {
                        return Some(action_plan);
                    } else {
                        eprintln!(
                            "AI 回复格式错误，第 {}/{} 次，无法解析为 RespActionPlan: {}",
                            attempt, max_attempts, resp.content
                        );
                    }
                }
                Err(e) => {
                    eprintln!("AI 请求错误，第 {}/{} 次: {}", attempt, max_attempts, e);
                }
            }
        }
        None
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
        let RespActionPlan {
            mind_state,
            actions,
        } = action_plan;
        let action_names = Self::action_names(&actions);
        self.execute_actions(incoming_message, actions).await;
        self.last_action_plan = Some(LastActionPlan {
            mind_state,
            action_names,
            updated_at_secs: Utc::now().timestamp(),
        });
    }

    /// 构建发送给聊天模型的完整上下文，包括系统提示词、聊天历史和当前指令。
    fn build_context(
        &self,
        incoming_message: &IncomingMessage,
        trigger_reason: &str,
        tool_results: Option<&[ToolActionResult]>,
    ) -> BuiltChatContext {
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

                if context
                    .last()
                    .is_some_and(|msg| msg.role == MessageRole::User)
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
            }
        }

        context.push(ContextMessage {
            role: MessageRole::User,
            content: self.render_instruction_prompt(trigger_reason, tool_results),
        });

        BuiltChatContext {
            messages: context,
            included_message_ids,
        }
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

    /// 渲染当前指令模板，替换时间、场景、回复决策状态和上一轮动作状态等动态占位符。
    fn render_instruction_prompt(
        &self,
        trigger_reason: &str,
        tool_results: Option<&[ToolActionResult]>,
    ) -> String {
        let now_secs = Utc::now().timestamp();
        let now_text = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let proactive_reply_percent = self
            .app_config
            .app
            .proactive_reply_percent
            .clamp(0.0, 100.0);
        let reply_decision_roll = rand::thread_rng().gen_range(0.0..100.0);
        let reply_decision_state = if reply_decision_roll < proactive_reply_percent {
            "更主动"
        } else {
            "更保守"
        };
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

        let mut prompt = self
            .app_config
            .prompt_config
            .instruction_prompt
            .replace("{{now}}", &now_text)
            .replace("{{last_action}}", &last_action)
            .replace("{{mind_state}}", &mind_state)
            .replace("{{scene}}", &self.scene)
            .replace("{{reply_decision_state}}", reply_decision_state)
            .replace(
                "{{max_history_messages}}",
                &self.app_config.app.max_history_messages.to_string(),
            )
            .replace("{{trigger_reason}}", trigger_reason);
        if let Some(tool_results) = tool_results {
            if !tool_results.is_empty() {
                Self::insert_tool_results_prompt(&mut prompt, tool_results);
            }
        }
        prompt
    }

    /// 把工具结果插入到当前状态和当前任务之间。
    fn insert_tool_results_prompt(prompt: &mut String, tool_results: &[ToolActionResult]) {
        let tool_results_text = format!(
            "\n# 工具结果\n{}\n",
            Self::render_tool_results(tool_results)
        );
        if let Some(index) = prompt.find("\n# 当前任务") {
            prompt.insert_str(index, &tool_results_text);
        } else {
            prompt.push_str(&tool_results_text);
        }
    }

    /// 渲染本轮工具结果。
    fn render_tool_results(tool_results: &[ToolActionResult]) -> String {
        tool_results
            .iter()
            .enumerate()
            .map(|(index, result)| {
                let status = if result.is_error { "失败" } else { "完成" };
                format!(
                    "{}. 动作：{}（{}）\n输入：{}\n结果：{}",
                    index + 1,
                    result.action_name,
                    status,
                    result.input_summary,
                    result.output
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// 按顺序执行模型返回的动作，每个动作由自己的方法处理副作用。
    async fn execute_actions(
        &mut self,
        incoming_message: &IncomingMessage,
        actions: Vec<RespAction>,
    ) {
        let mut next_message_ready_at_secs = incoming_message.timestamp as f64;
        let mut tool_tasks = Vec::new();
        for action in actions {
            match action {
                RespAction::SendMessage { text } => {
                    next_message_ready_at_secs = self
                        .execute_send_message(incoming_message, text, next_message_ready_at_secs)
                        .await;
                }
                RespAction::RecognizeImage { image_id, question } => {
                    tool_tasks.push(self.start_recognize_image(image_id, question));
                }
                RespAction::WebSearch { query } => {
                    tool_tasks.push(self.start_web_search(query));
                }
                RespAction::Remember { content } => {
                    self.execute_remember(content).await;
                }
                RespAction::WaitThenCheck {
                    delay_seconds,
                    reason,
                } => {
                    self.execute_wait_then_check(incoming_message, delay_seconds, reason);
                }
                RespAction::ScheduleTask { date, time, task } => {
                    self.execute_schedule_task(incoming_message, date, time, task);
                }
                RespAction::IgnoreMessages { duration_seconds } => {
                    self.execute_ignore_messages(incoming_message, duration_seconds);
                }
            }
        }
        if !tool_tasks.is_empty() {
            self.pending_tool_actions = Some(PendingToolActions {
                handle: tokio::spawn(Self::collect_tool_results(tool_tasks)),
                has_unread_messages: false,
                conversation_snapshot: incoming_message.clone(),
            });
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
            sleep(Duration::from_secs_f64(
                next_message_ready_at_secs - now_secs,
            ))
            .await;
        }

        self.transport_server
            .send_message(incoming_message.clone(), &text, self.db_manager.clone())
            .await
            .expect("发送回复失败");
        Utc::now().timestamp_millis() as f64 / 1000.0
    }

    /// 启动图片识别工具动作。
    fn start_recognize_image(&self, image_id: String, question: String) -> ToolActionTask {
        eprintln!(
            "安排图片识别动作: image_id={}, question={}",
            image_id, question
        );
        let app_config = self.app_config.clone();
        let db_manager = self.db_manager.clone();
        let task_image_id = image_id.clone();
        let task_question = question.clone();
        let handle = tokio::spawn(async move {
            MessageEnricher::answer_received_image_question(
                app_config,
                db_manager,
                &task_image_id,
                &task_question,
            )
            .await
        });
        ToolActionTask {
            action_name: "recognize_image".to_string(),
            input_summary: format!("图片ID：{}；问题：{}", image_id, question),
            handle,
        }
    }

    /// 启动联网搜索工具动作。
    fn start_web_search(&self, query: String) -> ToolActionTask {
        eprintln!("安排联网搜索动作: query={}", query);
        let app_config = self.app_config.clone();
        let task_query = query.clone();
        let handle = tokio::spawn(async move {
            let max_attempts = app_config.app.ai_request_max_attempts();
            let mut last_error = None;
            for attempt in 1..=max_attempts {
                let provider = app_config
                    .ai_providers
                    .get(&app_config.app.web_search_model_name)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "找不到联网搜索模型配置: {}",
                            app_config.app.web_search_model_name
                        )
                    })?;
                match ConversationWorker::run_ai_request_with_timeout(
                    app_config.app.ai_request_timeout_seconds,
                    "联网搜索 API 请求",
                    provider.web_search(&task_query),
                )
                .await
                {
                    Ok(result) => return Ok(result),
                    Err(err) => {
                        eprintln!(
                            "联网搜索 API 请求失败，第 {}/{} 次: {}",
                            attempt, max_attempts, err
                        );
                        last_error = Some(err);
                    }
                }
            }
            Err(last_error.expect("联网搜索重试循环至少应执行一次"))
        });
        ToolActionTask {
            action_name: "web_search".to_string(),
            input_summary: format!("查询：{}", query),
            handle,
        }
    }

    /// 等待本轮所有工具动作完成，并保持结果顺序和 action 顺序一致。
    async fn collect_tool_results(tool_tasks: Vec<ToolActionTask>) -> Vec<ToolActionResult> {
        let mut results = Vec::with_capacity(tool_tasks.len());
        for task in tool_tasks {
            let ToolActionTask {
                action_name,
                input_summary,
                handle,
            } = task;
            let (output, is_error) = match handle.await {
                Ok(Ok(output)) => (output, false),
                Ok(Err(err)) => (format!("工具执行失败：{}", err), true),
                Err(err) => (format!("工具任务异常：{}", err), true),
            };
            results.push(ToolActionResult {
                action_name,
                input_summary,
                output,
                is_error,
            });
        }
        results
    }

    /// 执行记忆写入动作；当前只记录日志，后续接入真实记忆库。
    async fn execute_remember(&self, content: String) {
        eprintln!("暂未实现记忆写入: content={}", content);
    }

    /// 执行等待后重新查看动作，直接更新当前 worker 的待触发状态。
    fn execute_wait_then_check(
        &mut self,
        incoming_message: &IncomingMessage,
        delay_seconds: u64,
        reason: String,
    ) {
        let delay_seconds = delay_seconds.clamp(
            WAIT_THEN_CHECK_DELAY_MIN_SECS,
            WAIT_THEN_CHECK_DELAY_MAX_SECS,
        );
        eprintln!(
            "安排等待后重新查看: delay_seconds={}, reason={}",
            delay_seconds, reason
        );
        self.pending_wait_check = Some(PendingWaitCheck {
            ready_at: Instant::now() + Duration::from_secs(delay_seconds),
            delay_seconds,
            reason,
            conversation_snapshot: incoming_message.clone(),
        });
    }

    /// 执行绝对时间定时任务动作，后台记录任务，到点后重新请求模型。
    fn execute_schedule_task(
        &mut self,
        incoming_message: &IncomingMessage,
        date: String,
        time: String,
        task: String,
    ) {
        let scheduled_at = match Self::parse_schedule_datetime(&date, &time) {
            Ok(scheduled_at) => scheduled_at,
            Err(err) => {
                eprintln!(
                    "安排定时任务失败: date={}, time={}, task={}，错误={}",
                    date, time, task, err
                );
                return;
            }
        };
        let now = Local::now();
        let delay = scheduled_at
            .signed_duration_since(now)
            .to_std()
            .unwrap_or_else(|_| Duration::from_secs(1));
        let ready_at = Instant::now() + delay;
        let scheduled_time_text = scheduled_at.format("%Y-%m-%d %H:%M:%S").to_string();
        eprintln!(
            "安排定时任务: time={}，delay_seconds={}，task={}",
            scheduled_time_text,
            delay.as_secs(),
            task
        );
        self.pending_scheduled_tasks.push(PendingScheduledTask {
            ready_at,
            scheduled_time_text,
            task,
            conversation_snapshot: incoming_message.clone(),
        });
    }

    fn parse_schedule_datetime(date: &str, time: &str) -> anyhow::Result<DateTime<Local>> {
        let date = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
            .map_err(|err| anyhow::anyhow!("日期格式错误，需要 YYYY-MM-DD：{}", err))?;
        let time = NaiveTime::parse_from_str(time.trim(), "%H:%M:%S")
            .map_err(|err| anyhow::anyhow!("时间格式错误，需要 HH:MM:SS：{}", err))?;
        let naive_datetime = date.and_time(time);
        Local
            .from_local_datetime(&naive_datetime)
            .single()
            .ok_or_else(|| anyhow::anyhow!("本地日期时间不存在或不唯一"))
    }

    /// 执行忽略消息动作：一段时间内新消息只入库，不触发 AI 请求。
    fn execute_ignore_messages(
        &mut self,
        incoming_message: &IncomingMessage,
        duration_seconds: u64,
    ) {
        let duration_seconds =
            duration_seconds.clamp(IGNORE_MESSAGES_MIN_SECS, IGNORE_MESSAGES_MAX_SECS);
        eprintln!("忽略后续消息: duration_seconds={}", duration_seconds);
        self.pending_wait_check = None;
        self.pending_ignore_messages = Some(PendingIgnoreMessages {
            ready_at: Instant::now() + Duration::from_secs(duration_seconds),
            duration_seconds,
            has_unread_messages: false,
            conversation_snapshot: incoming_message.clone(),
        });
    }

    /// 将本轮动作列表压缩成动作类型数组字符串，用于下一轮提示词中的上一轮动作。
    fn action_names(actions: &[RespAction]) -> String {
        if actions.is_empty() {
            "无动作".to_string()
        } else {
            let names: Vec<&str> = actions.iter().map(RespAction::action_name).collect();
            format!("[{}]", names.join(", "))
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
