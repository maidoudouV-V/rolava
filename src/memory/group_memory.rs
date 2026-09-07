use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;

use crate::repository::db_manager::QQChatContextManager;
use crate::transport::message::MessageTarget;

pub const MAX_GROUP_MEMORIES: usize = 50;
pub const MAX_TITLE_CHARS: usize = 50;
pub const MAX_CONTENT_CHARS: usize = 1000;
pub const MAX_RETENTION_DAYS: u16 = 365;
pub const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

/// 管理单个会话独立持有的群记忆。
pub struct GroupMemorySession {
    target: MessageTarget,
    db_manager: Arc<QQChatContextManager>,
}

impl GroupMemorySession {
    pub fn new(target: MessageTarget, db_manager: Arc<QQChatContextManager>) -> Self {
        Self { target, db_manager }
    }

    /// 新一轮主模型处理开始前，清理上一轮已经确认展示过的到期记忆。
    pub fn begin_turn(&self) -> Result<usize> {
        self.db_manager.delete_seen_expired_group_memories(
            &self.target.source,
            &self.target.bot_id,
            &self.target.conversation.id,
            Utc::now().timestamp(),
        )
    }

    /// 渲染当前会话的群记忆，并返回这次显示为“即将遗忘”的内部 ID。
    pub fn render_prompt(&self) -> Result<(String, Vec<i64>)> {
        let memories = self.db_manager.get_group_memories(
            &self.target.source,
            &self.target.bot_id,
            &self.target.conversation.id,
        )?;
        if memories.is_empty() {
            return Ok(("当前没有群记忆".to_string(), Vec::new()));
        }

        let now = Utc::now().timestamp();
        let mut prompt = String::new();
        let mut pending_expired_ids = Vec::new();
        for memory in memories {
            let remaining = if memory.expires_at == 0 {
                "长期（永不过期）".to_string()
            } else if memory.expires_at <= now {
                pending_expired_ids.push(memory.id);
                "0天（已到期，即将遗忘）".to_string()
            } else {
                let remaining_seconds = memory.expires_at - now;
                let remaining_days = (remaining_seconds + SECONDS_PER_DAY - 1) / SECONDS_PER_DAY;
                format!("{}天", remaining_days)
            };
            let content = memory.content.replace('\n', "\n    ");
            prompt.push_str(&format!(
                "---\n- 标题：{}\n- 内容：{}\n- 最后更新时间：{}\n- 剩余记忆时间：{}\n---\n",
                memory.title,
                content,
                super::format_memory_time(memory.updated_at)?,
                remaining
            ));
        }

        Ok((prompt.trim_end().to_string(), pending_expired_ids))
    }

    /// 新建或修改当前会话的群记忆。
    pub fn set_memory(
        &self,
        title: &str,
        content: Option<&str>,
        retention_days: Option<u16>,
    ) -> Result<String> {
        let title = title.trim();
        let content = content.map(str::trim);
        Self::validate_title(title)?;
        if let Some(content) = content {
            Self::validate_content(content)?;
        }
        if let Some(days) = retention_days {
            if days > MAX_RETENTION_DAYS {
                anyhow::bail!(
                    "群记忆时间必须在 0 到 {} 天之间，0 表示永不过期",
                    MAX_RETENTION_DAYS
                );
            }
        }

        let exists = self
            .db_manager
            .get_group_memories(
                &self.target.source,
                &self.target.bot_id,
                &self.target.conversation.id,
            )?
            .iter()
            .any(|memory| memory.title == title);
        if exists {
            if content.is_none() && retention_days.is_none() {
                anyhow::bail!("修改群记忆时至少提供 content 或 retention_days");
            }
        } else if content.is_none() || retention_days.is_none() {
            anyhow::bail!("新建群记忆时必须同时提供 content 和 retention_days");
        }

        let expires_at = retention_days.map(|days| {
            if days == 0 {
                0
            } else {
                Utc::now().timestamp() + i64::from(days) * SECONDS_PER_DAY
            }
        });
        let result = self.db_manager.set_group_memory(
            &self.target.source,
            &self.target.bot_id,
            &self.target.conversation.id,
            title,
            content,
            expires_at,
            MAX_GROUP_MEMORIES,
        )?;

        let action = if result.created { "添加" } else { "修改" };
        match result.evicted_title {
            Some(evicted_title) => Ok(format!(
                "群记忆“{}”{}成功；记忆数量超过 {} 条，已遗忘最快到期的“{}”",
                title, action, MAX_GROUP_MEMORIES, evicted_title
            )),
            None => Ok(format!("群记忆“{}”{}成功", title, action)),
        }
    }

    /// 删除当前会话中标题完全匹配的群记忆。
    pub fn delete_memory(&self, title: &str) -> Result<String> {
        let title = title.trim();
        Self::validate_title(title)?;
        let deleted = self.db_manager.delete_group_memory(
            &self.target.source,
            &self.target.bot_id,
            &self.target.conversation.id,
            title,
        )?;
        if !deleted {
            anyhow::bail!("找不到标题为“{}”的群记忆", title);
        }
        Ok(format!("群记忆“{}”删除成功", title))
    }

    /// 完整工具循环结束后，确认本轮展示后仍未续期的到期记忆。
    pub fn finish_turn(&self, displayed_expired_ids: &[i64]) -> Result<usize> {
        self.db_manager
            .mark_expired_group_memories_seen(displayed_expired_ids, Utc::now().timestamp())
    }

    pub fn validate_title(title: &str) -> Result<()> {
        if title.is_empty() {
            anyhow::bail!("群记忆标题不能为空");
        }
        if title.contains('\r') || title.contains('\n') {
            anyhow::bail!("群记忆标题必须是单行文本");
        }
        if title.chars().count() > MAX_TITLE_CHARS {
            anyhow::bail!("群记忆标题不能超过 {} 个字符", MAX_TITLE_CHARS);
        }
        Ok(())
    }

    pub fn validate_content(content: &str) -> Result<()> {
        if content.is_empty() {
            anyhow::bail!("群记忆内容不能为空");
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            anyhow::bail!("群记忆内容不能超过 {} 个字符", MAX_CONTENT_CHARS);
        }
        Ok(())
    }
}
