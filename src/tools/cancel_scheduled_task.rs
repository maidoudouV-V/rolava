use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{execution_not_implemented, parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"取消一个尚未触发的后台定时任务。

时间必须来自当前状态中已经计划的任务。相同时间存在多个任务时会一起取消。"#;

#[derive(Debug, Deserialize)]
pub struct CancelScheduledTaskArgs {
    pub time: String,
}

pub struct CancelScheduledTaskTool;

#[async_trait]
impl Tool for CancelScheduledTaskTool {
    fn name(&self) -> &'static str {
        "cancel_scheduled_task"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "time": {
                    "type": "string",
                    "description": "要取消的本地任务时间，格式 YYYY-MM-DD HH:MM:SS",
                    "pattern": "^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}$"
                }
            },
            "required": ["time"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, _context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let _arguments: CancelScheduledTaskArgs = parse_arguments(self.name(), arguments)?;

        /* 定时任务系统已移除。此处仅保留工具入口，后续会使用新方案重做。 */

        execution_not_implemented(self.name())
    }
}
