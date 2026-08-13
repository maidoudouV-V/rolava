use parking_lot::RwLock;
use serde::Serialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 管理页面只读的群资料；由启动同步和聊天流程主动填充。
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeGroupInfo {
    pub group_id: String,
    pub name: String,
    pub member_count: u64,
    pub max_member_count: Option<u64>,
}

/// OneBot 群成员资料。
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeGroupMember {
    pub user_id: String,
    pub nickname: String,
    pub card: String,
    pub role: String,
}

#[derive(Debug, Clone)]
pub struct CachedGroupMembers {
    pub members: Vec<RuntimeGroupMember>,
    pub fetched_at: Instant,
}

impl CachedGroupMembers {
    pub fn is_fresh(&self, ttl: Duration) -> bool {
        self.fetched_at.elapsed() < ttl
    }
}

/// 与会话 Actor 解耦的短期运行状态，管理页面只能读取或刷新平台缓存。
#[derive(Default)]
pub struct RuntimeState {
    bot_id: RwLock<Option<String>>,
    bot_name: RwLock<Option<String>>,
    groups: RwLock<HashMap<String, RuntimeGroupInfo>>,
    group_members: RwLock<HashMap<String, CachedGroupMembers>>,
    last_event_at: RwLock<Option<i64>>,
    onebot_online: RwLock<Option<bool>>,
}

impl RuntimeState {
    pub fn set_bot_id(&self, bot_id: impl Into<String>) {
        *self.bot_id.write() = Some(bot_id.into());
    }

    pub fn bot_id(&self) -> Option<String> {
        self.bot_id.read().clone()
    }

    pub fn set_bot_name(&self, bot_name: impl Into<String>) {
        let bot_name = bot_name.into();
        *self.bot_name.write() = (!bot_name.trim().is_empty()).then_some(bot_name);
    }

    pub fn bot_name(&self) -> Option<String> {
        self.bot_name.read().clone()
    }

    pub fn record_event(&self, timestamp: i64, bot_id: i64) {
        self.set_bot_id(bot_id.to_string());
        *self.last_event_at.write() = Some(timestamp);
    }

    pub fn last_event_at(&self) -> Option<i64> {
        *self.last_event_at.read()
    }

    pub fn set_onebot_online(&self, online: bool) {
        *self.onebot_online.write() = Some(online);
    }

    pub fn onebot_online(&self) -> Option<bool> {
        *self.onebot_online.read()
    }

    pub fn update_group(&self, group: RuntimeGroupInfo) {
        self.groups.write().insert(group.group_id.clone(), group);
    }

    pub fn group(&self, group_id: &str) -> Option<RuntimeGroupInfo> {
        self.groups.read().get(group_id).cloned()
    }

    pub fn groups(&self) -> Vec<RuntimeGroupInfo> {
        self.groups.read().values().cloned().collect()
    }

    pub fn cached_group_members(&self, group_id: &str) -> Option<CachedGroupMembers> {
        self.group_members.read().get(group_id).cloned()
    }

    pub fn cache_group_members(
        &self,
        group_id: impl Into<String>,
        members: Vec<RuntimeGroupMember>,
    ) {
        self.group_members.write().insert(
            group_id.into(),
            CachedGroupMembers {
                members,
                fetched_at: Instant::now(),
            },
        );
    }
}
