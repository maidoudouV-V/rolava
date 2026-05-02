use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize, Serialize)]
pub struct RespActionPlan {
    pub mind_state: String,
    pub actions: Vec<RespAction>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "action", content = "payload", rename_all = "snake_case")]
pub enum RespAction {
    SendMessage {
        text: String,
    },
    CallTool {
        tool: String,
        args: Value,
    },
    Remember {
        content: String,
    },
    ScheduleFollowUp {
        delay_seconds: u64,
        reason: String,
    },
}

impl RespAction {
    pub fn action_name(&self) -> &'static str {
        match self {
            RespAction::SendMessage { .. } => "send_message",
            RespAction::CallTool { .. } => "call_tool",
            RespAction::Remember { .. } => "remember",
            RespAction::ScheduleFollowUp { .. } => "schedule_follow_up",
        }
    }
}
