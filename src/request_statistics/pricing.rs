use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use parking_lot::RwLock;
use serde::Serialize;
use serde_json::Value;

use crate::ai_provider::ChatUsage;
use crate::model_metadata::{ModelMetadataStore, ModelPricing};

/// 请求完成时的参考单价与费用快照，单价为美元 / 百万 Token，费用为美元。
#[derive(Debug, Default, Serialize)]
pub struct RequestCost {
    pub input_price_per_million: Option<f64>,
    pub output_price_per_million: Option<f64>,
    pub cache_read_price_per_million: Option<f64>,
    pub estimated_cost_usd: Option<f64>,
}

impl RequestCost {
    pub fn estimate(usage: &ChatUsage, pricing: Option<ModelPricing>) -> Self {
        let Some(pricing) = pricing else {
            return Self::default();
        };
        let valid = |price: Option<f64>| price.filter(|price| price.is_finite() && *price >= 0.0);
        let mut result = Self {
            input_price_per_million: valid(pricing.input),
            output_price_per_million: valid(pricing.output),
            cache_read_price_per_million: valid(pricing.cache_read),
            estimated_cost_usd: None,
        };
        result.estimated_cost_usd = (|| {
            let input_price = result.input_price_per_million?;
            let output_price = result.output_price_per_million?;
            let input = usage.prompt_tokens?;
            let output = usage.completion_tokens?;
            let cached = match usage.cached_tokens {
                Some(cached) => cached,
                None if input == 0 || result.cache_read_price_per_million == Some(input_price) => 0,
                None => return None,
            };
            let uncached = input.checked_sub(cached)?;
            let cache_cost = if cached == 0 {
                0.0
            } else {
                cached as f64 * result.cache_read_price_per_million?
            };
            let cost = (uncached as f64 * input_price + cache_cost + output as f64 * output_price)
                / 1_000_000.0;
            cost.is_finite().then_some(cost)
        })();
        result
    }
}

/// OpenRouter 使用接口提供的单价，其余渠道使用已有模型资料库。
pub struct RequestPricing {
    metadata: Arc<ModelMetadataStore>,
    openrouter: RwLock<HashMap<String, HashMap<String, ModelPricing>>>,
}

impl RequestPricing {
    pub fn new(metadata: Arc<ModelMetadataStore>) -> Self {
        Self {
            metadata,
            openrouter: RwLock::new(HashMap::new()),
        }
    }

    pub fn lookup(&self, provider: &str, base_url: &str, model: &str) -> Option<ModelPricing> {
        if provider == "openrouter" {
            return self
                .openrouter
                .read()
                .get(openrouter_base(base_url))?
                .get(model)
                .cloned();
        }
        self.metadata
            .lookup(provider, base_url, model)
            .map(|metadata| metadata.pricing)
    }

    pub fn update_openrouter(
        &self,
        base_url: &str,
        prices: impl IntoIterator<Item = (String, ModelPricing)>,
    ) {
        self.openrouter.write().insert(
            openrouter_base(base_url).to_string(),
            prices.into_iter().collect(),
        );
    }

    pub async fn refresh_openrouter(&self, base_url: &str, key: &str) -> Result<()> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;
        let mut response = client
            .get(format!("{}/models", openrouter_base(base_url)))
            .bearer_auth(key)
            .send()
            .await?
            .error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len() + chunk.len() > 50 * 1024 * 1024 {
                bail!("OpenRouter 模型价格资料超过允许的文件大小");
            }
            bytes.extend_from_slice(&chunk);
        }
        let raw: Value = serde_json::from_slice(&bytes)?;
        let models = raw
            .get("data")
            .and_then(Value::as_array)
            .context("OpenRouter 模型价格响应缺少 data 数组")?;
        self.update_openrouter(
            base_url,
            models.iter().filter_map(|model| {
                Some((
                    model.get("id")?.as_str()?.to_string(),
                    ModelPricing::from_openrouter(model),
                ))
            }),
        );
        Ok(())
    }
}

fn openrouter_base(base_url: &str) -> &str {
    let base_url = base_url.trim().trim_end_matches('/');
    if base_url.is_empty() {
        "https://openrouter.ai/api/v1"
    } else {
        base_url
    }
}
