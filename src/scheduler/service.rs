use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone, Utc};
use rand::{distributions::Alphanumeric, Rng};
use tokio::sync::{mpsc, Notify};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::conversation_trigger::{ConversationTrigger, RoutedConversationTrigger};
use crate::repository::db_manager::QQChatContextManager;
use crate::transport::message::{Conversation, ConversationKind, MessageTarget};

use super::{calculate_next_run, model::validate_task_text, ScheduledTask};

const TASK_ID_RANDOM_CHARS: usize = 8;
const SCHEDULER_ERROR_RETRY_SECONDS: u64 = 5;

/// 定时任务的持久化入口和唯一调度循环，工具只通过该服务管理任务。
pub struct SchedulerService {
    db_manager: Arc<QQChatContextManager>,
    trigger_tx: mpsc::UnboundedSender<RoutedConversationTrigger>,
    schedule_changed: Notify,
}

impl SchedulerService {
    pub fn new(
        db_manager: Arc<QQChatContextManager>,
        trigger_tx: mpsc::UnboundedSender<RoutedConversationTrigger>,
    ) -> Self {
        Self {
            db_manager,
            trigger_tx,
            schedule_changed: Notify::new(),
        }
    }

    pub fn create_task(
        &self,
        target: &MessageTarget,
        title: &str,
        schedule: &str,
        instruction: &str,
    ) -> Result<ScheduledTask> {
        let title = title.trim();
        let schedule = schedule.trim();
        let instruction = instruction.trim();
        validate_task_text(title, instruction)?;
        let next_run_at = calculate_next_run(schedule, Local::now())?.timestamp();
        let task_id = self.generate_task_id()?;

        self.db_manager.insert_scheduled_task(
            &task_id,
            &target.source,
            &target.conversation.id,
            &target.bot_id,
            title,
            schedule,
            instruction,
            next_run_at,
        )?;
        self.schedule_changed.notify_one();
        self.get_task(target, &task_id)?
            .context("定时任务已经创建，但无法重新读取")
    }

    pub fn running_tasks(&self, target: &MessageTarget) -> Result<Vec<ScheduledTask>> {
        self.db_manager
            .get_running_scheduled_tasks(&target.source, &target.conversation.id)
    }

    pub fn get_task(&self, target: &MessageTarget, task_id: &str) -> Result<Option<ScheduledTask>> {
        self.db_manager
            .get_scheduled_task(&target.source, &target.conversation.id, task_id.trim())
    }

    pub fn update_task(
        &self,
        target: &MessageTarget,
        task_id: &str,
        title: Option<&str>,
        schedule: Option<&str>,
        instruction: Option<&str>,
    ) -> Result<ScheduledTask> {
        let task_id = task_id.trim();
        let existing = self
            .get_task(target, task_id)?
            .with_context(|| format!("当前会话中找不到定时任务 {}", task_id))?;
        let title = title.map(str::trim).unwrap_or(&existing.title);
        let schedule = schedule.map(str::trim).unwrap_or(&existing.schedule);
        let instruction = instruction.map(str::trim).unwrap_or(&existing.instruction);
        validate_task_text(title, instruction)?;

        // 每次更新都从当前时间重新计算，防止沿用旧表达式留下的执行位置。
        let next_run_at = calculate_next_run(schedule, Local::now())?.timestamp();
        let changed = self.db_manager.update_scheduled_task(
            &target.source,
            &target.conversation.id,
            task_id,
            title,
            schedule,
            instruction,
            next_run_at,
        )?;
        if !changed {
            anyhow::bail!("定时任务 {} 已被删除或修改失败", task_id);
        }
        self.schedule_changed.notify_one();
        self.get_task(target, task_id)?
            .context("定时任务已经更新，但无法重新读取")
    }

    pub fn delete_task(&self, target: &MessageTarget, task_id: &str) -> Result<bool> {
        let deleted = self.db_manager.delete_scheduled_task(
            &target.source,
            &target.conversation.id,
            task_id.trim(),
        )?;
        if deleted {
            self.schedule_changed.notify_one();
        }
        Ok(deleted)
    }

    /// 持续处理全部到期任务，并在任务表变化时立即重新计算睡眠时间。
    pub async fn run(self: Arc<Self>) {
        info!(timezone = %Local::now().offset(), "定时任务调度器已启动");
        let mut recovering_overdue_tasks = true;
        loop {
            if let Err(error) = self.process_due_tasks(recovering_overdue_tasks) {
                error!(error = %error, "处理到期定时任务失败");
                tokio::select! {
                    _ = sleep(Duration::from_secs(SCHEDULER_ERROR_RETRY_SECONDS)) => {}
                    _ = self.schedule_changed.notified() => {}
                }
                continue;
            }
            // 只有启动后的首次成功扫描属于重启恢复，之后到期的任务按正常触发处理。
            recovering_overdue_tasks = false;

            let next_run_at = match self.db_manager.next_scheduled_task_timestamp() {
                Ok(next_run_at) => next_run_at,
                Err(error) => {
                    error!(error = %error, "读取下一条定时任务失败");
                    sleep(Duration::from_secs(SCHEDULER_ERROR_RETRY_SECONDS)).await;
                    continue;
                }
            };

            match next_run_at {
                Some(timestamp) => {
                    let wait_seconds = timestamp.saturating_sub(Utc::now().timestamp()).max(0);
                    debug!(next_run_at = timestamp, wait_seconds, "等待下一条定时任务");
                    tokio::select! {
                        _ = sleep(Duration::from_secs(wait_seconds as u64)) => {}
                        _ = self.schedule_changed.notified() => {}
                    }
                }
                None => self.schedule_changed.notified().await,
            }
        }
    }

    fn process_due_tasks(&self, recovered_after_restart: bool) -> Result<()> {
        let now = Local::now();
        let due_tasks = self.db_manager.get_due_scheduled_tasks(now.timestamp())?;
        for task in due_tasks {
            let following_run_at = if task.schedule.starts_with("cron:") {
                Some(calculate_next_run(&task.schedule, now)?.timestamp())
            } else {
                None
            };

            // 先用原 next_run_at 条件认领，已被工具修改或删除的旧快照会自然失效。
            if !self.db_manager.claim_scheduled_task(
                &task.id,
                task.next_run_at,
                following_run_at,
            )? {
                continue;
            }

            let route = self.build_trigger(&task, now, recovered_after_restart)?;
            if let Err(error) = self.trigger_tx.send(route) {
                error!(task_id = %task.id, error = %error, "定时任务投递到会话分发器失败");
            } else {
                info!(task_id = %task.id, title = %task.title, "定时任务已触发");
            }
        }
        Ok(())
    }

    fn build_trigger(
        &self,
        task: &ScheduledTask,
        current_time: DateTime<Local>,
        recovered_after_restart: bool,
    ) -> Result<RoutedConversationTrigger> {
        let kind = match task.conversation_kind.as_str() {
            "direct" => ConversationKind::Direct,
            "group" => ConversationKind::Group,
            kind => anyhow::bail!("定时任务 {} 的会话类型无效: {}", task.id, kind),
        };
        let scheduled_time: DateTime<Local> = Local
            .timestamp_opt(task.next_run_at, 0)
            .single()
            .context("定时任务的下一次执行时间无效")?;
        let prompt = if recovered_after_restart {
            format!(
                "# 系统定时任务触发\n\n任务 ID：{}\n任务名称：{}\n计划触发时间：{}\n当前时间：{}\n延迟说明：程序启动时发现此任务已经超过计划触发时间，现在进行补充触发。任务原本要处理的情况可能已经变化或失去时效。\n\n任务说明：\n{}\n\n处理要求：\n请结合任务说明、当前时间和当前会话上下文，自行判断现在最合适的处理方式。不要因为任务已触发就机械执行已经过时的内容。",
                task.id,
                task.title,
                scheduled_time.format("%Y-%m-%d %H:%M:%S"),
                current_time.format("%Y-%m-%d %H:%M:%S"),
                task.instruction,
            )
        } else {
            format!(
                "# 系统定时任务触发\n\n任务 ID：{}\n任务名称：{}\n计划触发时间：{}\n\n任务说明：\n{}",
                task.id,
                task.title,
                scheduled_time.format("%Y-%m-%d %H:%M:%S"),
                task.instruction,
            )
        };

        Ok(RoutedConversationTrigger {
            target: MessageTarget {
                source: task.source.clone(),
                bot_id: task.bot_id.clone(),
                conversation: Conversation {
                    id: task.source_conversation_id.clone(),
                    kind,
                    title: task.conversation_title.clone(),
                },
            },
            trigger: ConversationTrigger {
                user_prompt: prompt,
            },
        })
    }

    fn generate_task_id(&self) -> Result<String> {
        for _ in 0..100 {
            let suffix: String = rand::thread_rng()
                .sample_iter(Alphanumeric)
                .take(TASK_ID_RANDOM_CHARS)
                .map(char::from)
                .collect();
            let task_id = format!("task_{}", suffix);
            if !self.db_manager.scheduled_task_id_exists(&task_id)? {
                return Ok(task_id);
            }
        }
        warn!("连续生成的定时任务 ID 均发生冲突");
        anyhow::bail!("无法生成唯一的定时任务 ID")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use chrono::Utc;
    use rand::Rng;
    use tokio::sync::mpsc;

    use crate::repository::db_manager::{NewChatMessage, QQChatContextManager};

    use super::SchedulerService;

    #[test]
    fn due_one_time_task_is_removed_and_routed() {
        let path = std::env::temp_dir().join(format!(
            "rolava-scheduler-test-{}-{}.db",
            std::process::id(),
            rand::thread_rng().gen::<u64>()
        ));
        let db_manager = Arc::new(QQChatContextManager::new(path.to_str().unwrap()).unwrap());
        db_manager
            .write_message(&NewChatMessage {
                source: "test".to_string(),
                source_conversation_id: "conversation".to_string(),
                conversation_kind: "group".to_string(),
                conversation_title: Some("测试会话".to_string()),
                conversation_metadata_json: "{}".to_string(),
                source_message_id: Some("message-1".to_string()),
                sender_id: "user".to_string(),
                sender_display_name: "用户".to_string(),
                sender_nickname: None,
                sender_role: None,
                content_text: "测试".to_string(),
                message_type: "text".to_string(),
                content_parts_json: "[]".to_string(),
                metadata_json: "{}".to_string(),
                event_timestamp: Utc::now().timestamp(),
            })
            .unwrap();
        db_manager
            .insert_scheduled_task(
                "task_A1b2C3d4",
                "test",
                "conversation",
                "bot",
                "测试提醒",
                "at:2020-01-01 00:00:00",
                "发送测试提醒",
                Utc::now().timestamp() - 1,
            )
            .unwrap();
        let (trigger_tx, mut trigger_rx) = mpsc::unbounded_channel();
        let scheduler = SchedulerService::new(db_manager.clone(), trigger_tx);

        scheduler.process_due_tasks(true).unwrap();

        let route = trigger_rx.try_recv().unwrap();
        assert_eq!(route.target.conversation.id, "conversation");
        assert!(route.trigger.user_prompt.contains("task_A1b2C3d4"));
        assert!(route.trigger.user_prompt.contains("当前时间："));
        assert!(route.trigger.user_prompt.contains("程序启动时发现"));
        assert!(route.trigger.user_prompt.contains("自行判断"));
        assert!(db_manager
            .get_running_scheduled_tasks("test", "conversation")
            .unwrap()
            .is_empty());

        db_manager
            .insert_scheduled_task(
                "task_E5f6G7h8",
                "test",
                "conversation",
                "bot",
                "正常提醒",
                "at:2020-01-01 00:00:00",
                "发送正常提醒",
                Utc::now().timestamp() - 1,
            )
            .unwrap();
        scheduler.process_due_tasks(false).unwrap();
        let normal_route = trigger_rx.try_recv().unwrap();
        assert!(!normal_route.trigger.user_prompt.contains("当前时间："));
        assert!(!normal_route.trigger.user_prompt.contains("延迟说明："));
        assert!(!normal_route.trigger.user_prompt.contains("自行判断"));

        drop(scheduler);
        drop(db_manager);
        fs::remove_file(path).unwrap();
    }
}
