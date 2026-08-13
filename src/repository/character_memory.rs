use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, OptionalExtension};

use super::db_manager::QQChatContextManager;

/// 一条按机器人账号和会话隔离的角色记忆。
#[derive(Debug, Clone)]
pub struct CharacterMemoryRecord {
    pub id: i64,
    pub title: String,
    pub content: String,
    pub expires_at: i64,
}

/// 新建或修改角色记忆后的持久化结果。
pub struct CharacterMemoryWriteResult {
    pub created: bool,
    pub evicted_title: Option<String>,
}

struct ExistingCharacterMemory {
    id: i64,
    content: String,
    expires_at: i64,
    expired_seen_at: Option<i64>,
}

impl QQChatContextManager {
    /// 读取当前会话的全部角色记忆，创建顺序保持稳定。
    pub fn get_character_memories(
        &self,
        source: &str,
        bot_id: &str,
        source_conversation_id: &str,
    ) -> Result<Vec<CharacterMemoryRecord>> {
        let connection = self.conn_pool.get()?;
        let mut statement = connection.prepare(
            "
            SELECT id, title, content, expires_at
            FROM character_memories
            WHERE source = ?1 AND bot_id = ?2 AND source_conversation_id = ?3
            ORDER BY id ASC
            ",
        )?;
        let records = statement
            .query_map(params![source, bot_id, source_conversation_id], |row| {
                Ok(CharacterMemoryRecord {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    content: row.get(2)?,
                    expires_at: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(records)
    }

    /// 原子地新建或修改角色记忆，并在新增超限时淘汰最快到期的旧记忆。
    pub fn set_character_memory(
        &self,
        source: &str,
        bot_id: &str,
        source_conversation_id: &str,
        title: &str,
        content: Option<&str>,
        expires_at: Option<i64>,
        max_memories: usize,
    ) -> Result<CharacterMemoryWriteResult> {
        let now = Utc::now().timestamp();
        let mut connection = self.conn_pool.get()?;
        let tx = connection.transaction()?;
        let existing = tx
            .query_row(
                "
                SELECT id, content, expires_at, expired_seen_at
                FROM character_memories
                WHERE source = ?1 AND bot_id = ?2
                  AND source_conversation_id = ?3 AND title = ?4
                ",
                params![source, bot_id, source_conversation_id, title],
                |row| {
                    Ok(ExistingCharacterMemory {
                        id: row.get(0)?,
                        content: row.get(1)?,
                        expires_at: row.get(2)?,
                        expired_seen_at: row.get(3)?,
                    })
                },
            )
            .optional()?;

        let (created, memory_id) = match existing {
            Some(existing) => {
                let next_content = content.unwrap_or(&existing.content);
                let next_expires_at = expires_at.unwrap_or(existing.expires_at);
                let next_expired_seen_at = if expires_at.is_some() {
                    None
                } else {
                    existing.expired_seen_at
                };
                tx.execute(
                    "
                    UPDATE character_memories
                    SET content = ?5, expires_at = ?6,
                        expired_seen_at = ?7, updated_at = ?8
                    WHERE source = ?1 AND bot_id = ?2
                      AND source_conversation_id = ?3 AND title = ?4
                    ",
                    params![
                        source,
                        bot_id,
                        source_conversation_id,
                        title,
                        next_content,
                        next_expires_at,
                        next_expired_seen_at,
                        now,
                    ],
                )?;
                (false, existing.id)
            }
            None => {
                let Some(content) = content else {
                    anyhow::bail!("新建角色记忆缺少内容");
                };
                let Some(expires_at) = expires_at else {
                    anyhow::bail!("新建角色记忆缺少期限");
                };
                tx.execute(
                    "
                    INSERT INTO character_memories (
                        source, bot_id, source_conversation_id, title, content,
                        expires_at, expired_seen_at, created_at, updated_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?7)
                    ",
                    params![
                        source,
                        bot_id,
                        source_conversation_id,
                        title,
                        content,
                        expires_at,
                        now,
                    ],
                )?;
                (true, tx.last_insert_rowid())
            }
        };

        let mut evicted_title = None;
        if created {
            let count = tx.query_row(
                "
                SELECT COUNT(*) FROM character_memories
                WHERE source = ?1 AND bot_id = ?2 AND source_conversation_id = ?3
                ",
                params![source, bot_id, source_conversation_id],
                |row| row.get::<_, i64>(0),
            )?;
            if count > max_memories as i64 {
                let mut statement = tx.prepare(
                    "
                    SELECT id, title
                    FROM character_memories
                    WHERE source = ?1 AND bot_id = ?2 AND source_conversation_id = ?3
                    ORDER BY expires_at ASC, created_at ASC, id ASC
                    LIMIT 2
                    ",
                )?;
                let candidates = statement
                    .query_map(params![source, bot_id, source_conversation_id], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(statement);

                // 新记忆若最先到期则淘汰第二名，否则直接淘汰最先到期项。
                let evicted = if candidates.first().is_some_and(|item| item.0 == memory_id) {
                    candidates.get(1)
                } else {
                    candidates.first()
                };
                if let Some((evicted_id, title)) = evicted {
                    tx.execute(
                        "DELETE FROM character_memories WHERE id = ?1",
                        params![evicted_id],
                    )?;
                    evicted_title = Some(title.clone());
                }
            }
        }

        tx.commit()?;
        Ok(CharacterMemoryWriteResult {
            created,
            evicted_title,
        })
    }

    /// 删除当前会话中由标题精确指定的角色记忆。
    pub fn delete_character_memory(
        &self,
        source: &str,
        bot_id: &str,
        source_conversation_id: &str,
        title: &str,
    ) -> Result<bool> {
        let changed = self.conn_pool.get()?.execute(
            "
            DELETE FROM character_memories
            WHERE source = ?1 AND bot_id = ?2
              AND source_conversation_id = ?3 AND title = ?4
            ",
            params![source, bot_id, source_conversation_id, title],
        )?;
        Ok(changed != 0)
    }

    /// 管理后台使用稳定内部 ID 修改标题、内容和期限，并重置到期展示状态。
    pub fn update_character_memory_by_id(
        &self,
        source: &str,
        bot_id: &str,
        source_conversation_id: &str,
        memory_id: i64,
        title: &str,
        content: &str,
        expires_at: i64,
    ) -> Result<bool> {
        let changed = self.conn_pool.get()?.execute(
            "UPDATE character_memories
             SET title = ?5, content = ?6, expires_at = ?7,
                 expired_seen_at = NULL, updated_at = ?8
             WHERE source = ?1 AND bot_id = ?2
               AND source_conversation_id = ?3 AND id = ?4",
            params![
                source,
                bot_id,
                source_conversation_id,
                memory_id,
                title,
                content,
                expires_at,
                Utc::now().timestamp(),
            ],
        )?;
        Ok(changed != 0)
    }

    pub fn delete_character_memory_by_id(
        &self,
        source: &str,
        bot_id: &str,
        source_conversation_id: &str,
        memory_id: i64,
    ) -> Result<bool> {
        let changed = self.conn_pool.get()?.execute(
            "DELETE FROM character_memories
             WHERE source = ?1 AND bot_id = ?2 AND source_conversation_id = ?3 AND id = ?4",
            params![source, bot_id, source_conversation_id, memory_id],
        )?;
        Ok(changed != 0)
    }

    /// 删除此前已经向主模型展示过“即将遗忘”的到期记忆。
    pub fn delete_seen_expired_character_memories(
        &self,
        source: &str,
        bot_id: &str,
        source_conversation_id: &str,
        now: i64,
    ) -> Result<usize> {
        Ok(self.conn_pool.get()?.execute(
            "
            DELETE FROM character_memories
            WHERE source = ?1 AND bot_id = ?2 AND source_conversation_id = ?3
              AND expires_at <= ?4 AND expired_seen_at IS NOT NULL
            ",
            params![source, bot_id, source_conversation_id, now],
        )?)
    }

    /// 只标记本轮确实随成功请求展示、并且之后仍未续期的到期记忆。
    pub fn mark_expired_character_memories_seen(
        &self,
        memory_ids: &[i64],
        now: i64,
    ) -> Result<usize> {
        let mut connection = self.conn_pool.get()?;
        let tx = connection.transaction()?;
        let mut changed = 0;
        // 数量上限为 50，逐条条件更新可以避免动态拼接 SQL 占位符。
        for memory_id in memory_ids {
            changed += tx.execute(
                "
                UPDATE character_memories
                SET expired_seen_at = ?2, updated_at = ?2
                WHERE id = ?1 AND expires_at <= ?2 AND expired_seen_at IS NULL
                ",
                params![memory_id, now],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }
}
