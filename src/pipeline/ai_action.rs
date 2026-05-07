use serde::{Deserialize, Serialize};

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
    RecognizeImage {
        image_id: String,
        question: String,
    },
    WebSearch {
        query: String,
    },
    Remember {
        content: String,
    },
    WaitThenCheck {
        delay_seconds: u64,
        reason: String,
    },
    ScheduleTask {
        date: String,
        time: String,
        task: String,
    },
    CancelScheduledTask {
        time: String,
    },
    IgnoreMessages {
        duration_seconds: u64,
    },
}

impl RespAction {
    pub fn action_name(&self) -> &'static str {
        match self {
            RespAction::SendMessage { .. } => "send_message",
            RespAction::RecognizeImage { .. } => "recognize_image",
            RespAction::WebSearch { .. } => "web_search",
            RespAction::Remember { .. } => "remember",
            RespAction::WaitThenCheck { .. } => "wait_then_check",
            RespAction::ScheduleTask { .. } => "schedule_task",
            RespAction::CancelScheduledTask { .. } => "cancel_scheduled_task",
            RespAction::IgnoreMessages { .. } => "ignore_messages",
        }
    }
}
