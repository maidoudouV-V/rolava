use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"删除当前会话中的一个运行中定时任务。

task_id 必须来自当前状态提供的运行中任务摘要，不要根据标题或时间猜测。需要先确认完整任务说明时调用 get_scheduled_task。删除成功后任务不会再触发。"#;

#[derive(Debug, Deserialize)]
pub struct DeleteScheduledTaskArgs {
    pub task_id: String,
}

pub struct DeleteScheduledTaskTool;

#[async_trait]
impl Tool for DeleteScheduledTaskTool {
    fn name(&self) -> &'static str {
        "delete_scheduled_task"
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
        let arguments: DeleteScheduledTaskArgs = parse_arguments(self.name(), arguments)?;
        let task_id = arguments.task_id.trim();
        if task_id.is_empty() {
            anyhow::bail!("task_id 不能为空");
        }
        if !context
            .services
            .scheduler
            .delete_task(&context.conversation.target, task_id)?
        {
            anyhow::bail!("当前会话中找不到定时任务 {}", task_id);
        }
        Ok(ToolOutput::text(format!(
            "定时任务删除成功：task_id={}",
            task_id
        )))
    }
}
