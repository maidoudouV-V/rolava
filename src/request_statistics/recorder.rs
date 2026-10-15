use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use serde_json::Value;
use tracing::error;

use super::{RequestCost, RequestPricing, RequestPurpose, RequestRecord, RequestStatisticsStore};
use crate::ai_provider::ChatUsage;

tokio::task_local! {
    static REQUEST_PURPOSE: RequestPurpose;
}

/// 用途随当前请求 Future 传递，避免共享模型被并发调用时互相覆盖。
pub async fn with_request_purpose<T>(
    purpose: RequestPurpose,
    request: impl Future<Output = T>,
) -> T {
    REQUEST_PURPOSE.scope(purpose, request).await
}

#[derive(Clone)]
pub struct RequestRecorder {
    store: Arc<RequestStatisticsStore>,
    provider_name: String,
    model_name: String,
    pricing: Option<(Arc<RequestPricing>, String)>,
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
            pricing: None,
        }
    }

    pub fn with_pricing(mut self, pricing: Arc<RequestPricing>, base_url: String) -> Self {
        self.pricing = Some((pricing, base_url));
        self
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
                purpose: REQUEST_PURPOSE
                    .try_with(|purpose| purpose.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                response_kind: None,
                outcome: "interrupted".into(),
                http_status: None,
                elapsed_ms: 0,
                usage: ChatUsage::default(),
                cost: RequestCost::default(),
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
                "openrouter" | "openai_compatible" => {
                    ChatUsage::from_chat_completions(&raw).unwrap_or_default()
                }
                "gemini" => ChatUsage::from_gemini(&raw).unwrap_or_default(),
                _ => ChatUsage::default(),
            };
            record.response_kind = response_kind(&record.provider_type, &raw).map(str::to_string);
        }
    }

    pub fn complete(&mut self, success: bool) {
        if let Some(record) = &mut self.record {
            record.outcome = if success { "success" } else { "failed" }.into();
        }
    }
}

fn response_kind(provider: &str, raw: &Value) -> Option<&'static str> {
    let (has_text, has_tools) = match provider {
        "gemini" => {
            let parts = raw.pointer("/candidates/0/content/parts")?.as_array()?;
            (
                parts.iter().any(|part| {
                    part.get("thought").and_then(Value::as_bool) != Some(true)
                        && part
                            .get("text")
                            .and_then(Value::as_str)
                            .is_some_and(|text| !text.trim().is_empty())
                }),
                parts
                    .iter()
                    .any(|part| part.get("functionCall").is_some_and(Value::is_object)),
            )
        }
        "openrouter" | "openai_compatible" => {
            let message = raw.pointer("/choices/0/message")?.as_object()?;
            (
                message
                    .get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty()),
                message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|calls| !calls.is_empty()),
            )
        }
        _ => return None,
    };
    Some(match (has_text, has_tools) {
        (true, true) => "text_tool",
        (true, false) => "text",
        (false, true) => "tool",
        (false, false) => "empty",
    })
}

impl Drop for RequestMeasurement {
    fn drop(&mut self) {
        if let (Some(recorder), Some(mut record)) = (self.recorder.take(), self.record.take()) {
            record.elapsed_ms = self.started.elapsed().as_millis().min(i64::MAX as u128) as u64;
            tokio::task::spawn_blocking(move || {
                if let Some((pricing, base_url)) = &recorder.pricing {
                    record.cost = RequestCost::estimate(
                        &record.usage,
                        pricing.lookup(&record.provider_type, base_url, &record.model),
                    );
                }
                if let Err(error) = recorder.store.insert(&record) {
                    error!(error = %format!("{error:#}"), "保存 AI 请求统计失败");
                }
            });
        }
    }
}
