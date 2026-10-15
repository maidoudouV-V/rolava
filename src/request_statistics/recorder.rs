use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use serde_json::Value;
use tracing::error;

use super::{RequestRecord, RequestStatisticsStore};
use crate::ai_provider::ChatUsage;

#[derive(Clone)]
pub struct RequestRecorder {
    store: Arc<RequestStatisticsStore>,
    provider_name: String,
    model_name: String,
}

impl RequestRecorder {
    pub fn new(
        store: Arc<RequestStatisticsStore>,
        provider_name: String,
        model_name: String,
    ) -> Self {
        Self {
            store,
            provider_name,
            model_name,
        }
    }
}

/// 请求被超时或取消丢弃时仍记录中断，记录写入在阻塞线程池中执行。
pub struct RequestMeasurement {
    recorder: Option<RequestRecorder>,
    record: Option<RequestRecord>,
    started: Instant,
}

impl RequestMeasurement {
    pub fn begin(
        recorder: Option<&RequestRecorder>,
        provider: &str,
        model: &str,
        operation: &str,
    ) -> Self {
        Self {
            recorder: recorder.cloned(),
            record: recorder.map(|recorder| RequestRecord {
                id: 0,
                started_at: Utc::now().timestamp(),
                provider_type: provider.to_string(),
                provider_name: recorder.provider_name.clone(),
                model_name: recorder.model_name.clone(),
                model: model.to_string(),
                operation: operation.to_string(),
                outcome: "interrupted".into(),
                http_status: None,
                elapsed_ms: 0,
                usage: ChatUsage::default(),
            }),
            started: Instant::now(),
        }
    }

    pub fn response_status(&mut self, status: u16) {
        if let Some(record) = &mut self.record {
            record.http_status = Some(status);
        }
    }

    pub fn response_body(&mut self, body: &str) {
        if let (Some(record), Ok(raw)) = (&mut self.record, serde_json::from_str::<Value>(body)) {
            if let Some(model) = raw
                .get("model")
                .or_else(|| raw.get("modelVersion"))
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
            {
                record.model = model.to_string();
            }
            record.usage = match record.provider_type.as_str() {
                "openrouter" => ChatUsage::from_openrouter(&raw).unwrap_or_default(),
                "gemini" => ChatUsage::from_gemini(&raw).unwrap_or_default(),
                _ => ChatUsage::default(),
            };
        }
    }

    pub fn complete(&mut self, success: bool) {
        if let Some(record) = &mut self.record {
            record.outcome = if success { "success" } else { "failed" }.into();
        }
    }
}

impl Drop for RequestMeasurement {
    fn drop(&mut self) {
        if let (Some(recorder), Some(mut record)) = (self.recorder.take(), self.record.take()) {
            record.elapsed_ms = self.started.elapsed().as_millis().min(i64::MAX as u128) as u64;
            tokio::task::spawn_blocking(move || {
                if let Err(error) = recorder.store.insert(&record) {
                    error!(error = %format!("{error:#}"), "保存 AI 请求统计失败");
                }
            });
        }
    }
}
