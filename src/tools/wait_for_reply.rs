use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::time::{sleep, Duration};
use tracing::{error, info, info_span, trace, Instrument};

use crate::config::render_prompt_template;
use crate::conversation_trigger::ConversationTrigger;

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"当需要等待对方回答或确认，并在对方迟迟未回复时继续处理，使用此工具。
工具会创建后台等待任务并立即返回，你可以继续完成本轮处理，无需再次调用来检查等待结果。
指定时间到期时，如果对方在当前会话中没有发送任何新消息，系统会再次唤起你，按 reason 中的说明继续处理；对方已发送消息则不触发本次超时处理。
同一会话中，对同一人的新等待任务会替换旧任务。"#;

#[derive(Debug, Deserialize)]
pub struct WaitForReplyArgs {
    pub user_id: String,
    pub timeout_seconds: u64,
    pub reason: String,
}

pub struct WaitForReplyTool {
    /// ToolRegistry 为每个会话独立创建；同一目标的新任务会替换旧任务。
    pending_tasks: Arc<Mutex<PendingWaitTasks>>,
}

#[derive(Default)]
struct PendingWaitTasks {
    next_task_id: u64,
    targets: HashMap<String, u64>,
}

impl PendingWaitTasks {
    fn should_trigger(
        &mut self,
        target: &str,
        task_id: u64,
        has_replied: impl FnOnce() -> Result<bool>,
    ) -> Result<bool> {
        if self.targets.get(target) != Some(&task_id) {
            return Ok(false);
        }
        self.targets.remove(target);
        Ok(!has_replied()?)
    }
}

impl WaitForReplyTool {
    pub fn new() -> Self {
        Self {
            pending_tasks: Arc::new(Mutex::new(PendingWaitTasks::default())),
        }
    }

    fn replace_target_task(&self, target: String) -> u64 {
        let mut pending = self.pending_tasks.lock();
        pending.next_task_id = pending.next_task_id.wrapping_add(1).max(1);
        let task_id = pending.next_task_id;
        pending.targets.insert(target, task_id);
        task_id
    }
}

#[async_trait]
impl Tool for WaitForReplyTool {
    fn name(&self) -> &'static str {
        "wait_for_reply"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "user_id": {
                    "type": "string",
                    "description": "需要等待回复的对方 QQ 号，使用最近活跃用户中显示的真实 QQ 号",
                    "minLength": 1
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "最长等待秒数，范围 10 到 3600",
                    "minimum": 10,
                    "maximum": 3600
                },
                "reason": {
                    "type": "string",
                    "description": "等待对方回复的原因，以及到期仍未回复时需要继续处理的事项",
                    "minLength": 1
                }
            },
            "required": ["user_id", "timeout_seconds", "reason"],
            "additionalProperties": false
        })
    }

    fn reset_conversation_state(&self) {
        // 清空目标映射后，已经休眠的后台任务会在醒来时自行退出。
        self.pending_tasks.lock().targets.clear();
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let arguments: WaitForReplyArgs = parse_arguments(self.name(), arguments)?;
        let target = arguments.user_id.trim().to_string();
        let reason = arguments.reason.trim().to_string();
        if target.is_empty() {
            anyhow::bail!("等待回复的用户 QQ 号不能为空");
        }
        if !(10..=3600).contains(&arguments.timeout_seconds) {
            anyhow::bail!("最长等待秒数必须在 10 到 3600 之间");
        }
        if reason.is_empty() {
            anyhow::bail!("等待回复的原因不能为空");
        }

        let source = context.conversation.target.source.clone();
        let conversation_id = context.conversation.target.conversation.id.clone();
        let after_message_id = context
            .services
            .db_manager
            .get_latest_conversation_message_id(&source, &conversation_id)?;

        let task_id = self.replace_target_task(target.clone());

        let timeout_seconds = arguments.timeout_seconds;
        let pending_tasks = self.pending_tasks.clone();
        let db_manager = context.services.db_manager.clone();
        let trigger_sender = context.conversation.trigger_sender.clone();
        let task_target = target.clone();
        let timeout_prompt = context
            .services
            .app_config
            .prompt_config
            .wait_for_reply_timeout_prompt
            .clone();

        info!(target = %target, timeout_seconds, "已创建等待回复任务");
        trace!(target = %target, reason = %reason, "等待回复完整原因");

        let task_span = info_span!("wait_for_reply", target = %target, timeout_seconds);
        tokio::spawn(
            async move {
                sleep(Duration::from_secs(timeout_seconds)).await;

                if pending_tasks.lock().targets.get(&task_target) != Some(&task_id) {
                    return;
                }

                let user_prompt = render_prompt_template(
                    &timeout_prompt,
                    &[("user_id", &task_target), ("reason", &reason)],
                )
                .trim()
                .to_string();
                let condition_target = task_target.clone();
                let condition_tasks = pending_tasks.clone();
                // 校验随事件进入同一会话队列，排在前面的回复先完成入库。
                let condition = Box::new(move || {
                    condition_tasks
                        .lock()
                        .should_trigger(&condition_target, task_id, || {
                            db_manager.has_sender_message_after(
                                &source,
                                &conversation_id,
                                &condition_target,
                                after_message_id,
                            )
                        })
                });
                if let Err(error) = trigger_sender.send_trigger(ConversationTrigger {
                    user_prompt,
                    memory_review: false,
                    condition: Some(condition),
                }) {
                    let mut pending = pending_tasks.lock();
                    if pending.targets.get(&task_target) == Some(&task_id) {
                        pending.targets.remove(&task_target);
                    }
                    error!(error = %format!("{error:#}"), "等待回复到期后触发会话失败");
                } else {
                    info!("等待回复到期，已排队等待会话校验");
                }
            }
            .instrument(task_span),
        );

        Ok(ToolOutput::text(format!(
            "等待 QQ {} 回复的任务添加成功",
            target
        )))
    }
}
