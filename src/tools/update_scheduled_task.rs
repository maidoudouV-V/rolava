use anyhow::Result;
use async_trait::async_trait;
use chrono::{Local, TimeZone};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"修改当前会话中已有定时任务的标题、时间表达式或完整任务说明。
task_id 必须来自当前状态提供的运行中任务摘要，不要根据标题或时间猜测。只传需要修改的字段，至少提供 title、schedule 或 instruction 中的一项；需要先查看完整任务说明时调用 get_scheduled_task。
schedule 支持单次时间 `at:YYYY-MM-DD HH:MM:SS`，或五段周期时间 `cron:分 时 日 月 周`，时间均按系统本地时区解释。无论修改哪个字段，更新成功后都会以当前时间为基准，根据最终的 schedule 重新计算下一次执行时间。"#;

#[derive(Debug, Deserialize)]
pub struct UpdateScheduledTaskArgs {
    pub task_id: String,
    pub title: Option<String>,
    pub schedule: Option<String>,
    pub instruction: Option<String>,
}

pub struct UpdateScheduledTaskTool;

#[async_trait]
impl Tool for UpdateScheduledTaskTool {
    fn name(&self) -> &'static str {
        "update_scheduled_task"
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
                },
                "title": {
                    "type": "string",
                    "description": "新的单行任务标题，省略则保持不变",
                    "minLength": 1,
                    "maxLength": 50
                },
                "schedule": {
                    "type": "string",
                    "description": "新的 at: 或五段 cron: 时间表达式，省略则保持不变",
                    "minLength": 1,
                    "maxLength": 100
                },
                "instruction": {
                    "type": "string",
                    "description": "新的完整任务说明，省略则保持不变",
                    "minLength": 1,
                    "maxLength": 1000
                }
            },
            "required": ["task_id"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let arguments: UpdateScheduledTaskArgs = parse_arguments(self.name(), arguments)?;
        if arguments.title.is_none()
            && arguments.schedule.is_none()
            && arguments.instruction.is_none()
        {
            anyhow::bail!("至少需要提供 title、schedule 或 instruction 中的一项");
        }
        let task = context.services.scheduler.update_task(
            &context.conversation.target,
            &arguments.task_id,
            arguments.title.as_deref(),
            arguments.schedule.as_deref(),
            arguments.instruction.as_deref(),
        )?;
        let next_run_at = Local
            .timestamp_opt(task.next_run_at, 0)
            .single()
            .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| task.next_run_at.to_string());
        Ok(ToolOutput::text(format!(
            "定时任务修改成功：task_id={}，title={}，schedule={}，next_run_at={}",
            task.id, task.title, task.schedule, next_run_at
        )))
    }
}
