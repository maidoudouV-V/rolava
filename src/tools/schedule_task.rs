use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{execution_not_implemented, parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"在指定的本地日期和时间安排一个后台任务。到达该时间后，系统会重新读取会话并再次请求模型。

适用于未来明确时间点的提醒、查看或后续决策。相同时间的未触发任务会更新为最新内容。"#;

#[derive(Debug, Deserialize)]
pub struct ScheduleTaskArgs {
    pub date: String,
    pub time: String,
    pub task: String,
}

pub struct ScheduleTaskTool;

#[async_trait]
impl Tool for ScheduleTaskTool {
    fn name(&self) -> &'static str {
        "schedule_task"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "date": {
                    "type": "string",
                    "description": "本地日期，格式 YYYY-MM-DD",
                    "pattern": "^[0-9]{4}-[0-9]{2}-[0-9]{2}$"
                },
                "time": {
                    "type": "string",
                    "description": "本地时间，格式 HH:MM:SS",
                    "pattern": "^[0-9]{2}:[0-9]{2}:[0-9]{2}$"
                },
                "task": {
                    "type": "string",
                    "description": "到达指定时间后需要重新考虑的任务"
                }
            },
            "required": ["date", "time", "task"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, _context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let _arguments: ScheduleTaskArgs = parse_arguments(self.name(), arguments)?;

        /* 定时任务系统已移除。此处仅保留工具入口，后续会使用新方案重做。 */

        execution_not_implemented(self.name())
    }
}
