use serde::Serialize;

use super::RequestCost;
use crate::ai_provider::ChatUsage;

#[derive(Clone, Copy)]
pub enum RequestPurpose {
    Chat,
    Filter,
    Vision,
    WebSearch,
    HistorySummary,
    MemoryReview,
    ModelTest,
}

impl RequestPurpose {
    pub fn is_valid(value: &str) -> bool {
        matches!(
            value,
            "chat"
                | "filter"
                | "vision"
                | "web_search"
                | "history_summary"
                | "memory_review"
                | "model_test"
                | "unknown"
        )
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Filter => "filter",
            Self::Vision => "vision",
            Self::WebSearch => "web_search",
            Self::HistorySummary => "history_summary",
            Self::MemoryReview => "memory_review",
            Self::ModelTest => "model_test",
        }
    }
}

/// 每次实际 API 请求的元信息与规范化用量，无法确定的字段保持为空。
#[derive(Debug, Serialize)]
pub struct RequestRecord {
    pub id: i64,
    pub started_at: i64,
    pub provider_type: String,
    pub provider_name: String,
    pub model_name: String,
    /// 响应中的实际模型 ID；未返回时使用请求模型 ID。
    pub model: String,
    pub operation: String,
    /// 每次调用的实际用途，同一配置模型可以用于不同任务。
    pub purpose: String,
    /// 根据实际响应区分正文与工具调用；未取得响应内容时为空。
    pub response_kind: Option<String>,
    pub outcome: String,
    pub http_status: Option<u16>,
    pub elapsed_ms: u64,
    #[serde(flatten)]
    pub usage: ChatUsage,
    #[serde(flatten)]
    pub cost: RequestCost,
}

#[derive(Debug, Serialize)]
pub struct RequestSummary {
    pub requests: u64,
    pub successful: u64,
    pub failed: u64,
    pub interrupted: u64,
    pub usage_reported: u64,
    #[serde(flatten)]
    pub usage: ChatUsage,
    pub cache_hit_rate: Option<f64>,
    pub cost_known_requests: u64,
    pub estimated_cost_usd: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct RequestStatisticsPage {
    pub items: Vec<RequestRecord>,
    pub summary: Option<RequestSummary>,
    pub next_cursor: Option<String>,
}
