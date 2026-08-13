use anyhow::{bail, Context, Result};
use reqwest::header::AUTHORIZATION;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tracing::trace;

use super::{OneBotGroupMessageDto, OneBotHttpServer, OneBotMessageDto, OneBotMessageEnvelopeDto};
use crate::transport::message::IncomingMessage;

const HISTORY_API_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
struct OneBotHistoryApiResponse<T> {
    status: String,
    retcode: i32,
    data: Option<T>,
    message: Option<String>,
    wording: Option<String>,
}

#[derive(Deserialize)]
struct OneBotLoginInfoDto {
    user_id: i64,
}

#[derive(Deserialize)]
struct OneBotGroupInfoDto {
    group_id: i64,
}

#[derive(Deserialize)]
struct OneBotGroupHistoryDataDto {
    #[serde(default)]
    messages: Vec<OneBotGroupHistoryMessageDto>,
}

/// 历史接口返回的是完整群消息对象，字段与实时群消息上报主体一致。
#[derive(Deserialize)]
struct OneBotGroupHistoryMessageDto {
    time: i64,
    #[serde(default)]
    message_seq: Option<Value>,
    #[serde(flatten)]
    message: OneBotGroupMessageDto,
}

impl OneBotGroupHistoryMessageDto {
    fn order_key(&self) -> (i64, i64, i64) {
        let message_seq = self
            .message_seq
            .as_ref()
            .and_then(value_as_i64)
            .unwrap_or(i64::from(self.message.message_id));
        (self.time, message_seq, i64::from(self.message.message_id))
    }
}

impl OneBotHttpServer {
    /// 查询当前 OneBot 登录账号，用于识别并截断机器人自己发送的历史消息。
    pub async fn fetch_login_user_id(&self) -> Result<i64> {
        let response: OneBotLoginInfoDto =
            self.call_history_api("get_login_info", json!({})).await?;
        Ok(response.user_id)
    }

    /// 查询当前机器人加入的全部群，供群白名单为空时确定历史同步目标。
    pub async fn fetch_group_ids(&self) -> Result<Vec<i64>> {
        let groups: Vec<OneBotGroupInfoDto> =
            self.call_history_api("get_group_list", json!({})).await?;
        Ok(groups.into_iter().map(|group| group.group_id).collect())
    }

    /// 获取指定群的最近历史，并复用实时上报的标准消息转换流程。
    pub async fn fetch_group_history(
        &self,
        group_id: i64,
        bot_id: i64,
        count: u32,
    ) -> Result<Vec<IncomingMessage>> {
        let payload = json!({
            "group_id": group_id.to_string(),
            "message_seq": 0,
            "count": count,
        });
        let response: OneBotGroupHistoryDataDto = self
            .call_history_api("get_group_msg_history", payload)
            .await?;
        let mut history = response.messages;
        history.sort_by_key(OneBotGroupHistoryMessageDto::order_key);

        let mut messages = Vec::with_capacity(history.len());
        for history_message in history {
            let envelope = OneBotMessageEnvelopeDto {
                time: history_message.time,
                self_id: bot_id,
                message: OneBotMessageDto::Group(history_message.message),
            };
            messages.push(envelope.into_incoming_message(self).await);
        }
        Ok(messages)
    }

    async fn call_history_api<T>(&self, api_path: &str, payload: Value) -> Result<T>
    where
        T: DeserializeOwned,
    {
        trace!(api_path, payload = %payload, "OneBot 历史同步完整请求");
        let mut request = self
            .client
            .post(format!("{}/{}", self.onebot_api_url, api_path))
            .timeout(HISTORY_API_TIMEOUT)
            .json(&payload);
        if let Some(token) = &self.onebot_token {
            request = request.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let response = request
            .send()
            .await
            .with_context(|| format!("调用 OneBot {} 接口失败", api_path))?;
        let http_status = response.status();
        let response_text = response
            .text()
            .await
            .with_context(|| format!("读取 OneBot {} 响应失败", api_path))?;
        trace!(api_path, status = %http_status, response = %response_text, "OneBot 历史同步原始响应");
        if !http_status.is_success() {
            bail!("OneBot {} 接口返回 HTTP {}", api_path, http_status);
        }

        let response: OneBotHistoryApiResponse<T> = serde_json::from_str(&response_text)
            .with_context(|| format!("解析 OneBot {} 响应失败", api_path))?;
        if response.retcode != 0 || response.status != "ok" {
            let detail = response
                .wording
                .filter(|text| !text.trim().is_empty())
                .or(response.message)
                .unwrap_or_else(|| "未提供错误信息".to_string());
            bail!(
                "OneBot {} 接口返回错误，status={}，retcode={}：{}",
                api_path,
                response.status,
                response.retcode,
                detail
            );
        }
        response
            .data
            .ok_or_else(|| anyhow::anyhow!("OneBot {} 接口未返回 data", api_path))
    }
}

fn value_as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| value.as_str()?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::{OneBotGroupHistoryDataDto, OneBotHistoryApiResponse};

    // 验证字符串类型的消息序号可解析并参与排序。
    #[test]
    fn parses_group_history_response_with_string_message_sequence() {
        let response: OneBotHistoryApiResponse<OneBotGroupHistoryDataDto> = serde_json::from_str(
            r#"{
                    "status": "ok",
                    "retcode": 0,
                    "data": {
                        "messages": [{
                            "time": 100,
                            "message_seq": "200",
                            "message_type": "group",
                            "sub_type": "normal",
                            "message_id": 300,
                            "group_id": 123456,
                            "user_id": 42,
                            "message": [{"type":"text","data":{"text":"hello"}}],
                            "raw_message": "hello",
                            "font": 0,
                            "sender": {"user_id":42,"nickname":"user"}
                        }]
                    },
                    "message": "",
                    "wording": "",
                    "stream": "normal-action"
                }"#,
        )
        .unwrap();

        let history = response.data.unwrap().messages;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].order_key(), (100, 200, 300));
    }
}
