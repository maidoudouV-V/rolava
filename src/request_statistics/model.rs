use serde::Serialize;

use crate::ai_provider::ChatUsage;

/// 每次实际 API 请求的元信息与供应商返回的用量，未返回的字段保持为空。
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
    pub outcome: String,
    pub http_status: Option<u16>,
    pub elapsed_ms: u64,
    #[serde(flatten)]
    pub usage: ChatUsage,
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
}

#[derive(Debug, Serialize)]
pub struct RequestStatisticsPage {
    pub items: Vec<RequestRecord>,
    pub summary: Option<RequestSummary>,
    pub next_cursor: Option<String>,
}
