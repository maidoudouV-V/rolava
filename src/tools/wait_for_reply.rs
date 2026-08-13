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

const DESCRIPTION: &str = r#"等待指定秒数，如果指定目标在此期间没有发送任何消息，系统将触发模型调用。
重复调用时，同一用户的新任务会替换旧任务。
当需要等待对方回复时使用，适合等待对方回答或确认。"#;

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
                    "description": "需要等待回复的用户 QQ 号，使用最近活跃用户中显示的真实 QQ 号",
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
                    "description": "等待目标回复的原因，以及到期后需要继续处理的事项",
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

                let replied = db_manager.has_sender_message_after(
                    &source,
                    &conversation_id,
                    &task_target,
                    after_message_id,
                );

                let is_current = {
                    let mut pending = pending_tasks.lock();
                    if pending.targets.get(&task_target) != Some(&task_id) {
                        false
                    } else {
                        pending.targets.remove(&task_target);
                        true
                    }
                };
                if !is_current {
                    return;
                }

                match replied {
                    Ok(true) => {
                        info!("目标已在等待期间回复，不再触发模型");
                    }
                    Ok(false) => {
                        let user_prompt = render_prompt_template(
                            &timeout_prompt,
                            &[("user_id", &task_target), ("reason", &reason)],
                        )
                        .trim()
                        .to_string();
                        if let Err(error) =
                            trigger_sender.send_trigger(ConversationTrigger { user_prompt })
                        {
                            error!(error = %error, "等待回复到期后触发会话失败");
                        } else {
                            info!("等待回复到期，已触发会话");
                        }
                    }
                    Err(error) => {
                        error!(error = %error, "检查目标是否在等待期间回复失败");
                    }
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
