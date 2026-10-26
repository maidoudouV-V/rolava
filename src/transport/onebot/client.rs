use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use tracing::trace;

#[derive(Deserialize, Debug)]
pub(crate) struct OneBotApiResponse<T> {
    #[serde(default)]
    pub(crate) status: Option<String>,
    pub(crate) retcode: i32,
    pub(crate) data: Option<T>,
    #[serde(default)]
    pub(crate) message: Option<String>,
    #[serde(default)]
    pub(crate) wording: Option<String>,
}

impl<T> OneBotApiResponse<T> {
    pub(crate) fn error_detail(&self) -> &str {
        self.wording
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .or_else(|| {
                self.message
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
            })
            .unwrap_or("未提供错误详情")
    }

    pub(crate) fn status_text(&self) -> &str {
        self.status.as_deref().unwrap_or("<missing>")
    }
}

#[derive(Deserialize)]
pub(crate) struct GroupMemberInfo {
    pub(crate) nickname: String,
    pub(crate) card: Option<String>,
}

/// 借用现有 HTTP 连接池，统一 OneBot 查询的鉴权和响应解析。
pub(crate) struct OneBotApiClient<'a> {
    client: &'a Client,
    base_url: &'a str,
    token: Option<&'a str>,
}

impl<'a> OneBotApiClient<'a> {
    pub(crate) fn new(client: &'a Client, base_url: &'a str, token: Option<&'a str>) -> Self {
        Self {
            client,
            base_url,
            token,
        }
    }

    pub(crate) async fn request<T: DeserializeOwned>(
        &self,
        action: &str,
        payload: &Value,
        timeout: Option<Duration>,
    ) -> Result<OneBotApiResponse<T>> {
        trace!(action, payload = %payload, "OneBot API 完整请求");
        let mut request = self
            .client
            .post(format!("{}/{}", self.base_url, action))
            .json(payload);
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        if let Some(token) = self.token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("调用 OneBot {} 接口失败", action))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .with_context(|| format!("读取 OneBot {} 响应失败", action))?;
        trace!(action, status = %status, response = %text, "OneBot API 原始响应");
        if !status.is_success() {
            bail!(
                "OneBot {} 接口返回 HTTP {}，响应正文：{}",
                action,
                status,
                text
            );
        }
        serde_json::from_str(&text)
            .with_context(|| format!("解析 OneBot {} 响应失败，原始响应：{}", action, text))
    }

    pub(crate) async fn group_member_info(
        &self,
        group_id: impl Serialize,
        user_id: impl Serialize,
    ) -> Result<Option<GroupMemberInfo>> {
        let response = self
            .request::<GroupMemberInfo>(
                "get_group_member_info",
                &json!({
                    "group_id": group_id, "user_id": user_id, "no_cache": false,
                }),
                None,
            )
            .await?;
        if response.retcode != 0 {
            bail!(
                "OneBot get_group_member_info 接口返回错误，status={}，retcode={}：{}",
                response.status_text(),
                response.retcode,
                response.error_detail()
            );
        }
        Ok(response.data)
    }
}
