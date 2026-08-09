use reqwest::header::AUTHORIZATION;
use reqwest::Client;
use serde::Deserialize;
use tracing::warn;

use crate::config::AppConfig;
use crate::transport::message::{ConversationKind, MessageTarget};

pub(super) struct MemberProfile {
    pub nickname: String,
    pub group_card: Option<String>,
}

#[derive(Deserialize)]
struct OneBotResponse<T> {
    retcode: i32,
    data: Option<T>,
}

#[derive(Deserialize)]
struct OneBotGroupMemberInfo {
    nickname: String,
    card: Option<String>,
}

/// 按需读取最近活跃用户的 QQ 昵称和群名片。
pub(super) struct OneBotMemberProfileClient {
    target: MessageTarget,
    client: Client,
    onebot_api_url: String,
    onebot_token: Option<String>,
}

impl OneBotMemberProfileClient {
    pub fn new(target: MessageTarget, app_config: &AppConfig) -> Self {
        Self {
            target,
            client: Client::new(),
            onebot_api_url: app_config
                .server
                .onebot_api
                .trim_end_matches('/')
                .to_string(),
            onebot_token: (!app_config.server.onebot_token.is_empty())
                .then(|| app_config.server.onebot_token.clone()),
        }
    }

    pub async fn fetch(&self, user_id: &str) -> Option<MemberProfile> {
        if self.target.source != "onebot"
            || !matches!(self.target.conversation.kind, ConversationKind::Group)
        {
            return None;
        }

        let mut request = self
            .client
            .post(format!("{}/get_group_member_info", self.onebot_api_url))
            .json(&serde_json::json!({
                "group_id": self.target.conversation.id,
                "user_id": user_id,
            }));
        if let Some(token) = &self.onebot_token {
            request = request.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                warn!(user_id, error = %error, "查询记忆成员资料失败");
                return None;
            }
        };
        let result = match response
            .json::<OneBotResponse<OneBotGroupMemberInfo>>()
            .await
        {
            Ok(result) => result,
            Err(error) => {
                warn!(user_id, error = %error, "解析记忆成员资料失败");
                return None;
            }
        };
        if result.retcode != 0 {
            warn!(
                user_id,
                retcode = result.retcode,
                "查询记忆成员资料返回错误"
            );
            return None;
        }

        result.data.map(|member| MemberProfile {
            nickname: member.nickname,
            group_card: member.card,
        })
    }
}
