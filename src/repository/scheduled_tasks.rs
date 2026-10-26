use super::db_manager::QQChatContextManager;
use crate::scheduler::ScheduledTask;
use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, OptionalExtension, Row};

impl QQChatContextManager {
    /// 创建一条属于指定会话的运行中定时任务。
    pub fn insert_scheduled_task(
        &self,
        task_id: &str,
        source: &str,
        source_conversation_id: &str,
        bot_id: &str,
        title: &str,
        schedule: &str,
        instruction: &str,
        next_run_at: i64,
    ) -> Result<()> {
        let now = Utc::now().timestamp();
        let connection = self.conn_pool.get()?;
        connection.execute(
            "
            INSERT INTO scheduled_tasks (
                id, conversation_id, bot_id, title, schedule, instruction,
                next_run_at, last_triggered_at, created_at, updated_at
            )
            SELECT ?1, c.id, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?9
            FROM conversations c
            WHERE c.source = ?2 AND c.source_conversation_id = ?3
            ",
            params![
                task_id,
                source,
                source_conversation_id,
                bot_id,
                title,
                schedule,
                instruction,
                next_run_at,
                now,
            ],
        )?;
        if connection.changes() == 0 {
            anyhow::bail!("当前会话尚未写入数据库，无法创建定时任务");
        }
        Ok(())
    }

    pub fn scheduled_task_id_exists(&self, task_id: &str) -> Result<bool> {
        let connection = self.conn_pool.get()?;
        let exists = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduled_tasks WHERE id = ?1)",
            params![task_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(exists != 0)
    }

    /// 返回当前会话的所有运行中任务，顺序与实际触发顺序一致。
    pub fn get_running_scheduled_tasks(
        &self,
        source: &str,
        source_conversation_id: &str,
    ) -> Result<Vec<ScheduledTask>> {
        let connection = self.conn_pool.get()?;
        let mut statement = connection.prepare(&format!(
            "{} WHERE c.source = ?1 AND c.source_conversation_id = ?2 ORDER BY st.next_run_at ASC, st.id ASC",
            Self::scheduled_task_select_sql()
        ))?;
        let tasks = statement
            .query_map(
                params![source, source_conversation_id],
                Self::scheduled_task_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(tasks)
    }

    pub fn get_scheduled_task(
        &self,
        source: &str,
        source_conversation_id: &str,
        task_id: &str,
    ) -> Result<Option<ScheduledTask>> {
        let connection = self.conn_pool.get()?;
        connection
            .query_row(
                &format!(
                    "{} WHERE c.source = ?1 AND c.source_conversation_id = ?2 AND st.id = ?3",
                    Self::scheduled_task_select_sql()
                ),
                params![source, source_conversation_id, task_id],
                Self::scheduled_task_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// 更新时同时替换规范化后的下一次执行时间，避免调度器继续使用旧时间。
    pub fn update_scheduled_task(
        &self,
        source: &str,
        source_conversation_id: &str,
        task_id: &str,
        title: &str,
        schedule: &str,
        instruction: &str,
        next_run_at: i64,
    ) -> Result<bool> {
        let connection = self.conn_pool.get()?;
        let changed = connection.execute(
            "
            UPDATE scheduled_tasks
            SET title = ?4,
                schedule = ?5,
                instruction = ?6,
                next_run_at = ?7,
                updated_at = ?8
            WHERE id = ?3
              AND conversation_id = (
                  SELECT id FROM conversations
                  WHERE source = ?1 AND source_conversation_id = ?2
              )
            ",
            params![
                source,
                source_conversation_id,
                task_id,
                title,
                schedule,
                instruction,
                next_run_at,
                Utc::now().timestamp(),
            ],
        )?;
        Ok(changed > 0)
    }

    pub fn delete_scheduled_task(
        &self,
        source: &str,
        source_conversation_id: &str,
        task_id: &str,
    ) -> Result<bool> {
        let connection = self.conn_pool.get()?;
        let changed = connection.execute(
            "
            DELETE FROM scheduled_tasks
            WHERE id = ?3
              AND conversation_id = (
                  SELECT id FROM conversations
                  WHERE source = ?1 AND source_conversation_id = ?2
              )
            ",
            params![source, source_conversation_id, task_id],
        )?;
        Ok(changed > 0)
    }

    pub fn next_scheduled_task_timestamp(&self) -> Result<Option<i64>> {
        let connection = self.conn_pool.get()?;
        connection
            .query_row("SELECT MIN(next_run_at) FROM scheduled_tasks", [], |row| {
                row.get(0)
            })
            .map_err(Into::into)
    }

    pub fn get_due_scheduled_tasks(&self, now_timestamp: i64) -> Result<Vec<ScheduledTask>> {
        let connection = self.conn_pool.get()?;
        let mut statement = connection.prepare(&format!(
            "{} WHERE st.next_run_at <= ?1 ORDER BY st.next_run_at ASC, st.id ASC",
            Self::scheduled_task_select_sql()
        ))?;
        let tasks = statement
            .query_map(params![now_timestamp], Self::scheduled_task_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(tasks)
    }

    /// 条件更新相当于认领到期任务；若任务已被修改或删除，旧调度结果不会再触发。
    pub fn claim_scheduled_task(
        &self,
        task_id: &str,
        expected_next_run_at: i64,
        following_run_at: Option<i64>,
    ) -> Result<bool> {
        let connection = self.conn_pool.get()?;
        let changed = match following_run_at {
            Some(next_run_at) => connection.execute(
                "
                UPDATE scheduled_tasks
                SET last_triggered_at = next_run_at,
                    next_run_at = ?3,
                    updated_at = ?4
                WHERE id = ?1 AND next_run_at = ?2
                ",
                params![
                    task_id,
                    expected_next_run_at,
                    next_run_at,
                    Utc::now().timestamp()
                ],
            )?,
            None => connection.execute(
                "DELETE FROM scheduled_tasks WHERE id = ?1 AND next_run_at = ?2",
                params![task_id, expected_next_run_at],
            )?,
        };
        Ok(changed > 0)
    }

    fn scheduled_task_select_sql() -> &'static str {
        "
        SELECT
            st.id,
            st.conversation_id,
            c.source,
            c.source_conversation_id,
            c.kind,
            c.title,
            st.bot_id,
            st.title,
            st.schedule,
            st.instruction,
            st.next_run_at,
            st.last_triggered_at,
            st.created_at,
            st.updated_at
        FROM scheduled_tasks st
        INNER JOIN conversations c ON c.id = st.conversation_id
        "
    }

    fn scheduled_task_from_row(row: &Row<'_>) -> rusqlite::Result<ScheduledTask> {
        Ok(ScheduledTask {
            id: row.get(0)?,
            conversation_id: row.get(1)?,
            source: row.get(2)?,
            source_conversation_id: row.get(3)?,
            conversation_kind: row.get(4)?,
            conversation_title: row.get(5)?,
            bot_id: row.get(6)?,
            title: row.get(7)?,
            schedule: row.get(8)?,
            instruction: row.get(9)?,
            next_run_at: row.get(10)?,
            last_triggered_at: row.get(11)?,
            created_at: row.get(12)?,
            updated_at: row.get(13)?,
        })
    }
}
