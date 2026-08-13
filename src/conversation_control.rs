use std::sync::atomic::{AtomicBool, Ordering};

/// 单个会话共享的控制状态，不依赖 ConversationActor。
#[derive(Debug, Default)]
pub struct ConversationControl {
    bypass_ai_filter: AtomicBool,
}

impl ConversationControl {
    pub fn ai_filter_bypassed(&self) -> bool {
        self.bypass_ai_filter.load(Ordering::Acquire)
    }

    pub fn set_ai_filter_bypassed(&self, enabled: bool) {
        self.bypass_ai_filter.store(enabled, Ordering::Release);
    }
}
