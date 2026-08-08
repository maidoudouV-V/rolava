use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"根据稳定任务 ID 查询当前会话中一个运行中定时任务的完整信息。

当前状态已经提供所有运行中任务的 task_id、title 和 schedule。需要查看某个任务的完整 instruction 或下一次执行时间时调用本工具，不要猜测任务 ID。"#;

#[derive(Debug, Deserialize)]
pub struct GetScheduledTaskArgs {
    pub task_id: String,
}

#[derive(Serialize)]
struct ScheduledTaskDetails<'a> {
    task_id: &'a str,
    title: &'a str,
    schedule: &'a str,
    instruction: &'a str,
    next_run_at: String,
}

pub struct GetScheduledTaskTool;

#[async_trait]
impl Tool for GetScheduledTaskTool {
    fn name(&self) -> &'static str {
        "get_scheduled_task"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "当前运行任务摘要中提供的稳定任务 ID",
                    "minLength": 1
                }
            },
            "required": ["task_id"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let arguments: GetScheduledTaskArgs = parse_arguments(self.name(), arguments)?;
        let task_id = arguments.task_id.trim();
        if task_id.is_empty() {
            anyhow::bail!("task_id 不能为空");
        }
        let task = context
            .services
            .scheduler
            .get_task(&context.conversation.target, task_id)?
            .with_context(|| format!("当前会话中找不到定时任务 {}", task_id))?;
        let details = ScheduledTaskDetails {
            task_id: &task.id,
            title: &task.title,
            schedule: &task.schedule,
            instruction: &task.instruction,
            next_run_at: Local
                .timestamp_opt(task.next_run_at, 0)
                .single()
                .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_else(|| task.next_run_at.to_string()),
        };
        Ok(ToolOutput::text(serde_json::to_string(&details)?))
    }
}
