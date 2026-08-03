use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{execution_not_implemented, parse_arguments, Tool, ToolContext, ToolOutput};

const DESCRIPTION: &str = r#"等待指定秒数；如果期间当前会话没有新消息，系统会再次请求模型查看会话。

适合等待对方补充、回答或确认。不要连续无限等待；重复调用时只保留最新设置。"#;

#[derive(Debug, Deserialize)]
pub struct WaitThenCheckArgs {
    pub delay_seconds: u64,
    pub reason: String,
}

pub struct WaitThenCheckTool;

#[async_trait]
impl Tool for WaitThenCheckTool {
    fn name(&self) -> &'static str {
        "wait_then_check"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "delay_seconds": {
                    "type": "integer",
                    "description": "等待秒数，范围 5 到 600",
                    "minimum": 5,
                    "maximum": 600
                },
                "reason": {
                    "type": "string",
                    "description": "安排本次等待的原因"
                }
            },
            "required": ["delay_seconds", "reason"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, _context: &ToolContext, arguments: &str) -> Result<ToolOutput> {
        let _arguments: WaitThenCheckArgs = parse_arguments(self.name(), arguments)?;

        /* 旧 worker 状态实现暂存。新的等待过滤器会位于消息主流程之前。
        let WaitThenCheckArgs {
            delay_seconds,
            reason,
        } = _arguments;
        let delay_seconds = delay_seconds.clamp(5, 600);
        context.pending_wait_check = Some(PendingWaitCheck {
            ready_at: Instant::now() + Duration::from_secs(delay_seconds),
            delay_seconds,
            reason,
            conversation_snapshot: context.incoming_message.clone(),
        });
        return Ok(ToolOutput {
            content: "已安排稍后重新查看".to_string(),
        });
        */

        execution_not_implemented(self.name())
    }
}
