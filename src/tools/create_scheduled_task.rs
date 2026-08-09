use anyhow::Result;
use async_trait::async_trait;
use chrono::{Local, TimeZone};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"为当前会话创建一个新的定时任务。
schedule 支持单次时间 `at:YYYY-MM-DD HH:MM:SS`，或五段周期时间 `cron:分 时 日 月 周`，例如每天 09:00 为 `cron:0 9 * * *`。时间均按系统本地时区解释，单次任务时间必须晚于当前时间。
title 必须是最多 50 个字符的单行标题；instruction 是任务触发时交给模型执行的完整说明，最多 1000 个字符。创建成功后会返回稳定的 task_id。"#;

#[derive(Debug, Deserialize)]
pub struct CreateScheduledTaskArgs {
    pub title: String,
    pub schedule: String,
    pub instruction: String,
}

pub struct CreateScheduledTaskTool;

#[async_trait]
impl Tool for CreateScheduledTaskTool {
    fn name(&self) -> &'static str {
        "create_scheduled_task"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "便于在运行任务摘要中识别任务的单行标题",
                    "minLength": 1,
                    "maxLength": 50
                },
                "schedule": {
                    "type": "string",
                    "description": "at:YYYY-MM-DD HH:MM:SS 或五段 cron:分 时 日 月 周",
                    "minLength": 1,
                    "maxLength": 100
                },
                "instruction": {
                    "type": "string",
                    "description": "任务到期触发时交给模型执行的完整事项说明",
                    "minLength": 1,
                    "maxLength": 1000
                }
            },
            "required": ["title", "schedule", "instruction"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let arguments: CreateScheduledTaskArgs = parse_arguments(self.name(), arguments)?;
        let task = context.services.scheduler.create_task(
            &context.conversation.target,
            &arguments.title,
            &arguments.schedule,
            &arguments.instruction,
        )?;
        let next_run_at = Local
            .timestamp_opt(task.next_run_at, 0)
            .single()
            .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| task.next_run_at.to_string());
        Ok(ToolOutput::text(format!(
            "定时任务创建成功：task_id={}，title={}，schedule={}，next_run_at={}",
            task.id, task.title, task.schedule, next_run_at
        )))
    }
}
