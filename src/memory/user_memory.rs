use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use rand::distributions::Alphanumeric;
use rand::Rng;
use serde_json::Value;

use crate::config::AppConfig;
use crate::repository::db_manager::{ChatMessage, QQChatContextManager};
use crate::transport::message::IncomingMessage;
use crate::transport::message::MessageTarget;

use super::member_profile::OneBotMemberProfileClient;

const INITIAL_MEMBER_MESSAGE_SCAN: usize = 10;
const MAX_ACTIVE_MEMORY_USERS: usize = 20;
const MEMORY_ID_RANDOM_CHARS: usize = 8;

/// 单个会话当前需要注入提示词的用户。
#[derive(Clone)]
struct ActiveMemoryUser {
    user_id: String,
    qq_nickname: String,
    group_card: Option<String>,
    last_relevant_message_id: i64,
}

struct ActiveUserCandidate {
    user_id: String,
    qq_nickname: Option<String>,
    group_card: Option<String>,
    last_relevant_message_id: i64,
}

/// 每个 ConversationActor 独立持有的用户记忆状态。
pub struct UserMemorySession {
    target: MessageTarget,
    db_manager: Arc<QQChatContextManager>,
    active_users: Mutex<Vec<ActiveMemoryUser>>,
    member_profile_client: OneBotMemberProfileClient,
}

impl UserMemorySession {
    pub fn new(
        target: MessageTarget,
        app_config: &AppConfig,
        db_manager: Arc<QQChatContextManager>,
    ) -> Self {
        let member_profile_client = OneBotMemberProfileClient::new(target.clone(), app_config);
        Self {
            target,
            db_manager,
            active_users: Mutex::new(Vec::new()),
            member_profile_client,
        }
    }

    pub fn reset(&self) {
        self.active_users.lock().clear();
    }

    /// 初始化时回溯最近十条人类消息；初始化完成后只处理本轮新消息。
    pub async fn refresh_active_users(
        &self,
        history: &[ChatMessage],
        current_messages: &[IncomingMessage],
        current_message_ids: &[i64],
    ) {
        let oldest_visible_message_id = history.first().map(|message| message.id);
        let initialize = {
            let mut active_users = self.active_users.lock();
            active_users.retain(|user| {
                oldest_visible_message_id
                    .is_some_and(|oldest| user.last_relevant_message_id >= oldest)
            });
            active_users.is_empty()
        };

        let mut candidates = if initialize {
            let selected_messages = history
                .iter()
                .rev()
                .filter(|message| message.sender_id != self.target.bot_id)
                .take(INITIAL_MEMBER_MESSAGE_SCAN)
                .collect::<Vec<_>>();
            self.collect_candidates(&selected_messages)
        } else {
            self.collect_current_candidates(current_messages, current_message_ids)
        };
        if candidates.is_empty() {
            return;
        }

        self.resolve_missing_profiles(&mut candidates).await;
        self.apply_candidates(candidates);
    }

    /// 渲染 instruction.md 中最近活跃用户的动态内容。
    pub fn render_prompt(&self) -> Result<String> {
        let active_users = self.active_users.lock().clone();
        if active_users.is_empty() {
            return Ok("当前没有最近活跃用户".to_string());
        }

        let mut output = String::new();
        for user in active_users {
            let memories = self.db_manager.get_user_memories(
                &self.target.source,
                &self.target.bot_id,
                &user.user_id,
            )?;
            output.push_str("---\n");
            output.push_str(&format!(
                "- 群名片：{}\n- QQ号：{}\n- QQ昵称：{}\n- 记忆：\n",
                user.group_card.as_deref().unwrap_or("无"),
                user.user_id,
                user.qq_nickname,
            ));
            if memories.is_empty() {
                output.push_str("    没有关于ta的记忆\n");
            } else {
                for memory in memories {
                    let content = memory.content.replace('\n', "\n        ");
                    output.push_str(&format!(
                        "    - ID：{}\n      内容：{}\n",
                        memory.memory_id, content
                    ));
                }
            }
            output.push_str("---\n");
        }
        Ok(output.trim_end().to_string())
    }

    /// 为活跃用户新增一条带稳定 ID 的记忆。
    pub fn create_memory(&self, user_id: &str, content: &str) -> Result<String> {
        let user_id = user_id.trim();
        let content = content.trim();
        self.ensure_active_user(user_id)?;
        if content.is_empty() {
            anyhow::bail!("记忆内容不能为空");
        }
        let memory_id = self.generate_memory_id()?;
        self.db_manager.insert_user_memory(
            &memory_id,
            &self.target.source,
            &self.target.bot_id,
            user_id,
            content,
        )?;
        Ok(format!("用户记忆添加成功，ID：{}", memory_id))
    }

    /// 使用提示词中显示的稳定 ID 更新已有记忆。
    pub fn update_memory(&self, user_id: &str, memory_id: &str, content: &str) -> Result<String> {
        let user_id = user_id.trim();
        let memory_id = memory_id.trim();
        let content = content.trim();
        self.ensure_active_user(user_id)?;
        if memory_id.is_empty() {
            anyhow::bail!("用户记忆 ID 不能为空");
        }
        if content.is_empty() {
            anyhow::bail!("记忆内容不能为空");
        }
        let updated = self.db_manager.update_user_memory(
            &self.target.source,
            &self.target.bot_id,
            user_id,
            memory_id,
            content,
        )?;
        if !updated {
            anyhow::bail!("找不到 QQ {} 的用户记忆 {}", user_id, memory_id);
        }
        Ok(format!("用户记忆 {} 修改成功", memory_id))
    }

    /// 删除活动用户的一条记忆。
    pub fn delete_memory(&self, user_id: &str, memory_id: &str) -> Result<String> {
        let user_id = user_id.trim();
        let memory_id = memory_id.trim();
        self.ensure_active_user(user_id)?;
        if memory_id.is_empty() {
            anyhow::bail!("用户记忆 ID 不能为空");
        }
        let deleted = self.db_manager.delete_user_memory(
            &self.target.source,
            &self.target.bot_id,
            user_id,
            memory_id,
        )?;
        if !deleted {
            anyhow::bail!("找不到 QQ {} 的用户记忆 {}", user_id, memory_id);
        }
        Ok(format!("用户记忆 {} 删除成功", memory_id))
    }

    fn collect_candidates(&self, messages: &[&ChatMessage]) -> Vec<ActiveUserCandidate> {
        let mut candidates = Vec::new();
        let mut candidate_indexes = HashMap::<String, usize>::new();

        for message in messages {
            Self::push_candidate(
                &mut candidates,
                &mut candidate_indexes,
                ActiveUserCandidate {
                    user_id: message.sender_id.clone(),
                    qq_nickname: Self::non_empty(message.sender_display_name.clone()),
                    group_card: message.sender_nickname.clone().and_then(Self::non_empty),
                    last_relevant_message_id: message.id,
                },
            );

            for user_id in Self::stored_message_mentions(message, &self.target.bot_id) {
                Self::push_candidate(
                    &mut candidates,
                    &mut candidate_indexes,
                    ActiveUserCandidate {
                        user_id,
                        qq_nickname: None,
                        group_card: None,
                        last_relevant_message_id: message.id,
                    },
                );
            }
        }
        candidates
    }

    fn collect_current_candidates(
        &self,
        messages: &[IncomingMessage],
        message_ids: &[i64],
    ) -> Vec<ActiveUserCandidate> {
        let mut candidates = Vec::new();
        let mut candidate_indexes = HashMap::<String, usize>::new();

        for (message, message_id) in messages.iter().zip(message_ids).rev() {
            if message.sender.id == self.target.bot_id {
                continue;
            }
            Self::push_candidate(
                &mut candidates,
                &mut candidate_indexes,
                ActiveUserCandidate {
                    user_id: message.sender.id.clone(),
                    qq_nickname: Self::non_empty(message.sender.display_name.clone()),
                    group_card: message.sender.nickname.clone().and_then(Self::non_empty),
                    last_relevant_message_id: *message_id,
                },
            );
            for user_id in message
                .content
                .parts
                .iter()
                .filter(|part| part.kind == "at")
                .filter_map(|part| part.data.get("qq"))
                .filter_map(|value| Self::mentioned_user_id(value, &self.target.bot_id))
            {
                Self::push_candidate(
                    &mut candidates,
                    &mut candidate_indexes,
                    ActiveUserCandidate {
                        user_id,
                        qq_nickname: None,
                        group_card: None,
                        last_relevant_message_id: *message_id,
                    },
                );
            }
        }
        candidates
    }

    fn push_candidate(
        candidates: &mut Vec<ActiveUserCandidate>,
        indexes: &mut HashMap<String, usize>,
        candidate: ActiveUserCandidate,
    ) {
        if let Some(index) = indexes.get(&candidate.user_id).copied() {
            let existing = &mut candidates[index];
            existing.last_relevant_message_id = existing
                .last_relevant_message_id
                .max(candidate.last_relevant_message_id);
            if existing.qq_nickname.is_none() {
                existing.qq_nickname = candidate.qq_nickname;
            }
            if existing.group_card.is_none() {
                existing.group_card = candidate.group_card;
            }
            return;
        }

        indexes.insert(candidate.user_id.clone(), candidates.len());
        candidates.push(candidate);
    }

    fn stored_message_mentions(message: &ChatMessage, bot_id: &str) -> Vec<String> {
        let Ok(Value::Array(parts)) = serde_json::from_str(&message.content_parts_json) else {
            return Vec::new();
        };
        parts
            .iter()
            .filter(|part| part.get("kind").and_then(Value::as_str) == Some("at"))
            .filter_map(|part| part.get("data").and_then(|data| data.get("qq")))
            .filter_map(|value| Self::mentioned_user_id(value, bot_id))
            .collect()
    }

    fn mentioned_user_id(value: &Value, bot_id: &str) -> Option<String> {
        let user_id = match value {
            Value::String(user_id) => user_id.clone(),
            Value::Number(user_id) => user_id.to_string(),
            _ => return None,
        };
        (user_id != "all" && user_id != bot_id).then_some(user_id)
    }

    async fn resolve_missing_profiles(&self, candidates: &mut [ActiveUserCandidate]) {
        let active_profiles = self
            .active_users
            .lock()
            .iter()
            .map(|user| (user.user_id.clone(), user.clone()))
            .collect::<HashMap<_, _>>();

        for candidate in candidates {
            if candidate.qq_nickname.is_some() {
                continue;
            }
            if let Some(active) = active_profiles.get(&candidate.user_id) {
                candidate
                    .qq_nickname
                    .get_or_insert_with(|| active.qq_nickname.clone());
                if candidate.group_card.is_none() {
                    candidate.group_card = active.group_card.clone();
                }
                continue;
            }
            if let Some(profile) = self.member_profile_client.fetch(&candidate.user_id).await {
                candidate.qq_nickname = Self::non_empty(profile.nickname);
                candidate.group_card = profile.group_card.and_then(Self::non_empty);
            }
        }
    }

    fn apply_candidates(&self, candidates: Vec<ActiveUserCandidate>) {
        let priority_user_ids = candidates
            .iter()
            .take(MAX_ACTIVE_MEMORY_USERS)
            .map(|candidate| candidate.user_id.clone())
            .collect::<HashSet<_>>();
        let mut active_users = self.active_users.lock();

        for candidate in candidates {
            if let Some(existing) = active_users
                .iter_mut()
                .find(|user| user.user_id == candidate.user_id)
            {
                existing.last_relevant_message_id = existing
                    .last_relevant_message_id
                    .max(candidate.last_relevant_message_id);
                if let Some(nickname) = candidate.qq_nickname {
                    existing.qq_nickname = nickname;
                    existing.group_card = candidate.group_card;
                }
                continue;
            }
            if !priority_user_ids.contains(&candidate.user_id) {
                continue;
            }

            let new_user = ActiveMemoryUser {
                qq_nickname: candidate
                    .qq_nickname
                    .unwrap_or_else(|| candidate.user_id.clone()),
                user_id: candidate.user_id,
                group_card: candidate.group_card,
                last_relevant_message_id: candidate.last_relevant_message_id,
            };
            if active_users.len() < MAX_ACTIVE_MEMORY_USERS {
                active_users.push(new_user);
                continue;
            }

            // 满员时只替换未出现在本轮候选中的最旧成员，其余位置保持不变。
            let replacement_index = active_users
                .iter()
                .enumerate()
                .filter(|(_, user)| !priority_user_ids.contains(&user.user_id))
                .min_by_key(|(_, user)| user.last_relevant_message_id)
                .map(|(index, _)| index);
            if let Some(index) = replacement_index {
                active_users[index] = new_user;
            }
        }
    }

    fn ensure_active_user(&self, user_id: &str) -> Result<()> {
        if user_id.is_empty() {
            anyhow::bail!("用户 QQ 号不能为空");
        }
        if !self
            .active_users
            .lock()
            .iter()
            .any(|user| user.user_id == user_id)
        {
            anyhow::bail!("QQ {} 不在当前最近活跃用户中", user_id);
        }
        Ok(())
    }

    fn generate_memory_id(&self) -> Result<String> {
        let mut rng = rand::thread_rng();
        for _ in 0..20 {
            let suffix = (&mut rng)
                .sample_iter(Alphanumeric)
                .take(MEMORY_ID_RANDOM_CHARS)
                .map(char::from)
                .collect::<String>();
            let memory_id = format!("mem_{}", suffix);
            if !self.db_manager.user_memory_id_exists(&memory_id)? {
                return Ok(memory_id);
            }
        }
        anyhow::bail!("生成用户记忆 ID 连续碰撞")
    }

    fn non_empty(value: String) -> Option<String> {
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    }
}
