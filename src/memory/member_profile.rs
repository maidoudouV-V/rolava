use crate::transport::onebot::client::OneBotApiClient;
use reqwest::Client;
use tracing::warn;

use crate::config::AppConfig;
use crate::transport::message::{ConversationKind, MessageTarget};

pub(super) struct MemberProfile {
    pub nickname: String,
    pub group_card: Option<String>,
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
        let api = OneBotApiClient::new(
            &self.client,
            &self.onebot_api_url,
            self.onebot_token.as_deref(),
        );
        match api
            .group_member_info(&self.target.conversation.id, user_id)
            .await
        {
            Ok(member) => member.map(|member| MemberProfile {
                nickname: member.nickname,
                group_card: member.card,
            }),
            Err(error) => {
                warn!(user_id, error = %format!("{error:#}"), "查询记忆成员资料失败");
                None
            }
        }
    }
}
