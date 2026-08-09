use anyhow::Result;
use chrono::Utc;
use rusqlite::params;

use super::db_manager::QQChatContextManager;

/// 一条按机器人账号和 QQ 用户隔离的长期用户记忆。
#[derive(Debug, Clone)]
pub struct UserMemoryRecord {
    /// 暴露给模型使用的稳定短 ID。
    pub memory_id: String,
    /// 记忆正文。
    pub content: String,
}

impl QQChatContextManager {
    /// 判断模型可见的用户记忆 ID 是否已经存在。
    pub fn user_memory_id_exists(&self, memory_id: &str) -> Result<bool> {
        let connection = self.conn_pool.get()?;
        let exists = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM user_memories WHERE memory_id = ?1)",
            params![memory_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(exists != 0)
    }

    /// 新增用户记忆；内部主键确保新记忆始终排在旧记忆之前。
    pub fn insert_user_memory(
        &self,
        memory_id: &str,
        source: &str,
        bot_id: &str,
        user_id: &str,
        content: &str,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        let connection = self.conn_pool.get()?;
        connection.execute(
            "
            INSERT INTO user_memories (
                memory_id, source, bot_id, user_id, content, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
            ",
            params![memory_id, source, bot_id, user_id, content, now],
        )?;
        Ok(())
    }

    /// 修改指定用户的一条稳定 ID 记忆，修改内容不会改变原有顺序。
    pub fn update_user_memory(
        &self,
        source: &str,
        bot_id: &str,
        user_id: &str,
        memory_id: &str,
        content: &str,
    ) -> Result<bool> {
        let changed = self.conn_pool.get()?.execute(
            "
            UPDATE user_memories
            SET content = ?5, updated_at = ?6
            WHERE source = ?1 AND bot_id = ?2 AND user_id = ?3 AND memory_id = ?4
            ",
            params![
                source,
                bot_id,
                user_id,
                memory_id,
                content,
                Utc::now().timestamp()
            ],
        )?;
        Ok(changed != 0)
    }

    /// 删除指定用户的一条稳定 ID 记忆。
    pub fn delete_user_memory(
        &self,
        source: &str,
        bot_id: &str,
        user_id: &str,
        memory_id: &str,
    ) -> Result<bool> {
        let changed = self.conn_pool.get()?.execute(
            "
            DELETE FROM user_memories
            WHERE source = ?1 AND bot_id = ?2 AND user_id = ?3 AND memory_id = ?4
            ",
            params![source, bot_id, user_id, memory_id],
        )?;
        Ok(changed != 0)
    }

    /// 按创建顺序从新到旧读取指定 QQ 用户的全部记忆。
    pub fn get_user_memories(
        &self,
        source: &str,
        bot_id: &str,
        user_id: &str,
    ) -> Result<Vec<UserMemoryRecord>> {
        let connection = self.conn_pool.get()?;
        let mut statement = connection.prepare(
            "
            SELECT memory_id, content
            FROM user_memories
            WHERE source = ?1 AND bot_id = ?2 AND user_id = ?3
            ORDER BY id DESC
            ",
        )?;
        let records = statement
            .query_map(params![source, bot_id, user_id], |row| {
                Ok(UserMemoryRecord {
                    memory_id: row.get(0)?,
                    content: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(records)
    }
}
