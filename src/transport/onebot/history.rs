use anyhow::{bail, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

use super::{OneBotGroupMessageDto, OneBotHttpServer, OneBotMessageDto, OneBotMessageEnvelopeDto};
use crate::runtime_state::{RuntimeGroupInfo, RuntimeGroupMember};
use crate::transport::message::IncomingMessage;

const HISTORY_API_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
struct OneBotLoginInfoDto {
    user_id: i64,
    #[serde(default)]
    nickname: String,
}

#[derive(Deserialize)]
struct OneBotGroupInfoDto {
    group_id: i64,
    #[serde(default)]
    group_name: String,
    #[serde(default)]
    member_count: u64,
    #[serde(default)]
    max_member_count: Option<u64>,
}

#[derive(Deserialize)]
struct OneBotGroupMemberDto {
    user_id: i64,
    #[serde(default)]
    nickname: String,
    #[serde(default)]
    card: String,
    #[serde(default)]
    role: String,
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
        self.runtime_state.set_bot_id(response.user_id.to_string());
        self.runtime_state.set_bot_name(response.nickname);
        Ok(response.user_id)
    }

    /// 查询当前机器人加入的全部群，供群白名单为空时确定历史同步目标。
    pub async fn fetch_group_ids(&self) -> Result<Vec<i64>> {
        let groups: Vec<OneBotGroupInfoDto> =
            self.call_history_api("get_group_list", json!({})).await?;
        Ok(groups
            .into_iter()
            .map(|group| {
                self.runtime_state.update_group(RuntimeGroupInfo {
                    group_id: group.group_id.to_string(),
                    name: group.group_name,
                    member_count: group.member_count,
                    max_member_count: group.max_member_count,
                });
                group.group_id
            })
            .collect())
    }

    /// 查询群成员列表；缓存策略由管理服务决定，此处只负责平台协议转换。
    pub async fn fetch_group_members(&self, group_id: i64) -> Result<Vec<RuntimeGroupMember>> {
        let members: Vec<OneBotGroupMemberDto> = self
            .call_history_api("get_group_member_list", json!({ "group_id": group_id }))
            .await?;
        Ok(members
            .into_iter()
            .map(|member| RuntimeGroupMember {
                user_id: member.user_id.to_string(),
                nickname: member.nickname,
                card: member.card,
                role: member.role,
            })
            .collect())
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
        let response = self
            .api_client()
            .request::<T>(api_path, &payload, Some(HISTORY_API_TIMEOUT))
            .await?;
        if response.retcode != 0 || response.status.as_deref() != Some("ok") {
            bail!(
                "OneBot {} 接口返回错误，status={}，retcode={}：{}",
                api_path,
                response.status_text(),
                response.retcode,
                response.error_detail()
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
