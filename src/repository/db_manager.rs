use crate::scheduler::ScheduledTask;
use crate::transport::message::{ConversationKind, IncomingMessage};
use anyhow::Result;
use chrono::Utc;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, params_from_iter, OptionalExtension, Row, Transaction};
use serde_json::{json, Value};

/// 一条已经写入数据库的消息标识。
#[derive(Debug, Clone, Copy)]
pub struct StoredMessage {
    /// messages 表主键。
    pub id: i64,
}

/// 清空单个会话聊天历史后的删除统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResetConversationResult {
    pub deleted_messages: usize,
}

/// 主模型本轮使用的数据库消息窗口及其统一分块容量。
#[derive(Debug, Clone)]
pub struct ConversationHistoryWindow {
    pub messages: Vec<ChatMessage>,
    pub history_block_size: usize,
}

/// 一条会话目录记录。
#[derive(Debug, Clone)]
pub struct ConversationRecord {
    /// 会话表主键。
    pub id: i64,
    /// 消息来源平台，例如 `onebot`。
    pub source: String,
    /// 来源平台上的会话 ID。
    pub source_conversation_id: String,
    /// 会话类型，例如 `direct` / `group`。
    pub kind: String,
    /// 会话展示名称。
    pub title: Option<String>,
    /// 会话扩展字段 JSON。
    pub metadata_json: String,
    /// 记录创建时间。
    pub created_at: i64,
    /// 最近一条消息的事件时间。
    pub last_message_at: i64,
}

/// 管理页面会话列表所需的只读摘要。
#[derive(Debug, Clone)]
pub struct AdminConversationSummary {
    pub conversation: ConversationRecord,
    pub latest_sender_name: Option<String>,
    pub latest_content: Option<String>,
    pub latest_message_at: Option<i64>,
    pub unread_count: u64,
}

/// 管理后台概览统计，不包含内部队列等实现状态。
#[derive(Debug, Clone, Copy, Default)]
pub struct AdminDatabaseStats {
    pub conversations: u64,
    pub group_conversations: u64,
    pub direct_conversations: u64,
    pub messages_today: u64,
    pub user_memories: u64,
    pub character_memories: u64,
    pub scheduled_tasks: u64,
}

/// 一条聊天记录。
#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// 消息表主键。
    pub id: i64,
    /// 所属会话表主键。
    pub conversation_id: i64,
    /// 消息来源平台。
    pub source: String,
    /// 来源平台上的会话 ID。
    pub source_conversation_id: String,
    /// 会话类型。
    pub conversation_kind: String,
    /// 来源平台上的原始消息 ID。
    pub source_message_id: Option<String>,
    /// 发送者 ID。
    pub sender_id: String,
    /// 发送者显示名。
    pub sender_display_name: String,
    /// 发送者昵称或名片。
    pub sender_nickname: Option<String>,
    /// 发送者角色。
    pub sender_role: Option<String>,
    /// 纯文本内容缓存。
    pub content_text: Option<String>,
    /// 主消息片段类型。
    pub message_type: String,
    /// 富文本消息片段 JSON。
    pub content_parts_json: String,
    /// 消息扩展字段 JSON。
    pub metadata_json: String,
    /// 是否已经被加入上下文并成功完成过一次 AI 请求。
    pub is_read: bool,
    /// 平台事件时间戳。
    pub event_timestamp: i64,
    /// 入库时间。
    pub created_at: i64,
}

/// 回复引用只需要的原消息摘要。
pub struct ReferencedMessage {
    pub sender_display_name: String,
    pub content_text: Option<String>,
}

/// 一条待写入的通用聊天记录。
#[derive(Debug, Clone)]
pub struct NewChatMessage {
    /// 消息来源平台。
    pub source: String,
    /// 来源平台上的会话 ID。
    pub source_conversation_id: String,
    /// 会话类型。
    pub conversation_kind: String,
    /// 会话展示名称。
    pub conversation_title: Option<String>,
    /// 会话扩展字段 JSON。
    pub conversation_metadata_json: String,
    /// 来源平台上的原始消息 ID。
    pub source_message_id: Option<String>,
    /// 发送者 ID。
    pub sender_id: String,
    /// 发送者显示名。
    pub sender_display_name: String,
    /// 发送者昵称或名片。
    pub sender_nickname: Option<String>,
    /// 发送者角色。
    pub sender_role: Option<String>,
    /// 纯文本内容缓存。
    pub content_text: String,
    /// 主消息片段类型。
    pub message_type: String,
    /// 富文本消息片段 JSON。
    pub content_parts_json: String,
    /// 消息扩展字段 JSON。
    pub metadata_json: String,
    /// 平台事件时间戳。
    pub event_timestamp: i64,
}

/// 已接收图片记录。
#[derive(Debug, Clone)]
pub struct ReceivedImageRecord {
    /// 图片短 ID，用于提示词和后续引用。
    pub image_id: String,
    /// 图片内容哈希，用于去重。
    pub content_hash: String,
    /// 图片本地保存路径。
    pub local_path: String,
    /// 图片内容简短描述。
    pub description: String,
}

/// 管理页面读取图片内容所需的最小资源信息。
pub struct AdminImageResource {
    pub local_path: String,
    pub mime_type: Option<String>,
}

/// 一条待写入的接收图片记录。
#[derive(Debug, Clone)]
pub struct NewReceivedImage {
    /// 图片短 ID。
    pub image_id: String,
    /// 图片内容哈希。
    pub content_hash: String,
    /// 图片本地保存路径。
    pub local_path: String,
    /// 图片原始下载地址。
    pub original_url: Option<String>,
    /// 图片 MIME 类型。
    pub mime_type: Option<String>,
    /// 图片文件大小，单位字节。
    pub file_size: i64,
    /// 图片内容简短描述。
    pub description: String,
    /// 扩展信息 JSON。
    pub metadata_json: String,
}

impl ConversationRecord {
    /// 将数据库行转换为会话目录记录。
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            source: row.get(1)?,
            source_conversation_id: row.get(2)?,
            kind: row.get(3)?,
            title: row.get(4)?,
            metadata_json: row.get(5)?,
            created_at: row.get(6)?,
            last_message_at: row.get(7)?,
        })
    }
}

impl ChatMessage {
    /// 将数据库行转换为聊天记录。
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            conversation_id: row.get(1)?,
            source: row.get(2)?,
            source_conversation_id: row.get(3)?,
            conversation_kind: row.get(4)?,
            source_message_id: row.get(5)?,
            sender_id: row.get(6)?,
            sender_display_name: row.get(7)?,
            sender_nickname: row.get(8)?,
            sender_role: row.get(9)?,
            content_text: row.get(10)?,
            message_type: row.get(11)?,
            content_parts_json: row.get(12)?,
            metadata_json: row.get(13)?,
            is_read: row.get::<_, i64>(14)? != 0,
            event_timestamp: row.get(15)?,
            created_at: row.get(16)?,
        })
    }
}

/// 聊天记录数据库管理器。
pub struct QQChatContextManager {
    /// SQLite 连接池。
    pub(super) conn_pool: Pool<SqliteConnectionManager>,
}

impl QQChatContextManager {
    /// 创建数据库管理器，并确保表和索引已经初始化。
    pub fn new(db_path: &str) -> Result<Self> {
        let manager = SqliteConnectionManager::file(db_path);
        let conn_pool = Pool::builder().max_size(5).build(manager)?;
        let conn = conn_pool.get()?;

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS conversations (
                id                      INTEGER PRIMARY KEY AUTOINCREMENT,
                source                  TEXT    NOT NULL,
                source_conversation_id  TEXT    NOT NULL,
                kind                    TEXT    NOT NULL,
                title                   TEXT,
                metadata_json           TEXT    NOT NULL DEFAULT '{}',
                created_at              INTEGER NOT NULL,
                last_message_at         INTEGER NOT NULL,
                UNIQUE(source, source_conversation_id)
            );

            CREATE TABLE IF NOT EXISTS messages (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                conversation_id     INTEGER NOT NULL,
                source_message_id   TEXT,
                sender_id           TEXT    NOT NULL,
                sender_display_name TEXT    NOT NULL DEFAULT '',
                sender_nickname     TEXT,
                sender_role         TEXT,
                content_text        TEXT,
                message_type        TEXT    NOT NULL DEFAULT 'text',
                content_parts_json  TEXT    NOT NULL DEFAULT '[]',
                metadata_json       TEXT    NOT NULL DEFAULT '{}',
                is_read             INTEGER NOT NULL DEFAULT 0,
                event_timestamp     INTEGER NOT NULL,
                created_at          INTEGER NOT NULL,
                FOREIGN KEY (conversation_id) REFERENCES conversations(id)
            );

            CREATE INDEX IF NOT EXISTS idx_conversations_last_message_at
            ON conversations (last_message_at DESC);

            CREATE INDEX IF NOT EXISTS idx_messages_conversation_id
            ON messages (conversation_id, id DESC);

            CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_conversation_source_message
            ON messages (conversation_id, source_message_id);

            CREATE TABLE IF NOT EXISTS scheduled_tasks (
                -- 稳定任务 ID；修改时间或内容时保持不变。
                id                  TEXT    PRIMARY KEY,
                -- 任务所属会话，只允许当前会话查询和修改。
                conversation_id     INTEGER NOT NULL,
                -- 投递内部触发时使用的机器人账号 ID。
                bot_id              TEXT    NOT NULL,
                -- 注入提示词的简短单行标题，最多 50 个字符。
                title               TEXT    NOT NULL,
                -- 原始时间表达式，格式为 at:... 或 cron:...。
                schedule            TEXT    NOT NULL,
                -- 到期时临时交给主模型的完整任务说明，最多 1000 个字符。
                instruction         TEXT    NOT NULL,
                -- 下一次执行的 Unix 时间戳；调度查询统一按绝对时间比较。
                next_run_at         INTEGER NOT NULL,
                -- 最近一次成功认领的计划执行时间。
                last_triggered_at   INTEGER,
                created_at          INTEGER NOT NULL,
                updated_at          INTEGER NOT NULL,
                FOREIGN KEY (conversation_id) REFERENCES conversations(id)
            );

            CREATE INDEX IF NOT EXISTS idx_scheduled_tasks_next_run_at
            ON scheduled_tasks (next_run_at ASC);

            CREATE TABLE IF NOT EXISTS user_memories (
                -- 内部自增主键只负责稳定排序，不暴露给模型。
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                -- 模型修改和删除记忆时使用的稳定随机 ID。
                memory_id   TEXT    NOT NULL UNIQUE,
                -- 平台、机器人账号和用户账号共同确定一份用户记忆。
                source      TEXT    NOT NULL,
                bot_id      TEXT    NOT NULL,
                user_id     TEXT    NOT NULL,
                content     TEXT    NOT NULL,
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_user_memories_owner
            ON user_memories (source, bot_id, user_id, id DESC);

            CREATE TABLE IF NOT EXISTS character_memories (
                -- 内部主键用于稳定排序和标记本轮实际展示的记忆。
                id                          INTEGER PRIMARY KEY AUTOINCREMENT,
                -- 平台、机器人账号和会话共同确定一份独立角色记忆。
                source                      TEXT    NOT NULL,
                bot_id                      TEXT    NOT NULL,
                source_conversation_id      TEXT    NOT NULL,
                -- 标题是模型修改和删除记忆时使用的会话内唯一键。
                title                       TEXT    NOT NULL,
                content                     TEXT    NOT NULL,
                -- 到期时间使用 Unix 时间戳；到期后仍需给主模型一次续期机会。
                expires_at                  INTEGER NOT NULL,
                -- 主模型看过“即将遗忘”且本轮没有续期后才写入。
                expired_seen_at             INTEGER,
                created_at                  INTEGER NOT NULL,
                updated_at                  INTEGER NOT NULL,
                UNIQUE (source, bot_id, source_conversation_id, title)
            );

            CREATE INDEX IF NOT EXISTS idx_character_memories_owner
            ON character_memories (
                source, bot_id, source_conversation_id, expires_at ASC, id ASC
            );

            CREATE TABLE IF NOT EXISTS received_images (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                image_id        TEXT    NOT NULL UNIQUE,
                content_hash    TEXT    NOT NULL UNIQUE,
                local_path      TEXT    NOT NULL,
                original_url    TEXT,
                mime_type       TEXT,
                file_size       INTEGER NOT NULL,
                description     TEXT    NOT NULL,
                metadata_json   TEXT    NOT NULL DEFAULT '{}',
                created_at      INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_received_images_content_hash
            ON received_images (content_hash);
            ",
        )?;
        Ok(Self { conn_pool })
    }

    /// 将通用入站消息转换为标准写入请求。
    pub fn new_message_from_incoming(message: &IncomingMessage) -> NewChatMessage {
        let message_type = message
            .content
            .parts
            .first()
            .map(|part| part.kind.clone())
            .unwrap_or_else(|| "text".to_string());
        let content_parts_json = Value::Array(
            message
                .content
                .parts
                .iter()
                .map(|part| {
                    json!({
                        "kind": part.kind,
                        "data": part.data
                    })
                })
                .collect(),
        )
        .to_string();

        NewChatMessage {
            source: message.source.clone(),
            source_conversation_id: message.conversation.id.clone(),
            conversation_kind: Self::conversation_kind_as_str(&message.conversation.kind)
                .to_string(),
            conversation_title: message.conversation.title.clone(),
            conversation_metadata_json: "{}".to_string(),
            source_message_id: message.message_id.clone(),
            sender_id: message.sender.id.clone(),
            sender_display_name: message.sender.display_name.clone(),
            sender_nickname: message.sender.nickname.clone(),
            sender_role: message.sender.role.clone(),
            content_text: message.content.text.clone(),
            message_type,
            content_parts_json,
            metadata_json: message.metadata.to_string(),
            event_timestamp: message.timestamp,
        }
    }

    /// 将一条通用入站消息写入数据库。
    pub fn write_incoming_message(&self, message: &IncomingMessage) -> Result<StoredMessage> {
        let new_message = Self::new_message_from_incoming(message);
        self.write_message_internal(&new_message)
    }

    /// 幂等写入平台消息；相同会话中的平台消息 ID 已存在时返回 `None`。
    pub fn write_incoming_message_if_new(
        &self,
        message: &IncomingMessage,
    ) -> Result<Option<StoredMessage>> {
        let new_message = Self::new_message_from_incoming(message);
        self.write_message_internal_if_new(&new_message)
    }

    /// 在执行多媒体增强前检查平台消息是否已存在。
    pub fn incoming_message_exists(&self, message: &IncomingMessage) -> Result<bool> {
        let Some(source_message_id) = Self::normalize_optional_text(message.message_id.as_deref())
        else {
            return Ok(false);
        };
        let connection = self.conn_pool.get()?;
        let exists = connection.query_row(
            "
            SELECT EXISTS(
                SELECT 1
                FROM messages m
                INNER JOIN conversations c ON c.id = m.conversation_id
                WHERE c.source = ?1
                  AND c.source_conversation_id = ?2
                  AND m.source_message_id = ?3
            )
            ",
            params![&message.source, &message.conversation.id, source_message_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(exists != 0)
    }

    /// 写入一条标准化后的聊天记录。
    pub fn write_message(&self, message: &NewChatMessage) -> Result<StoredMessage> {
        self.write_message_internal(message)
    }

    fn write_message_internal(&self, message: &NewChatMessage) -> Result<StoredMessage> {
        self.write_message_internal_with_conflict_policy(message, false)?
            .ok_or_else(|| anyhow::anyhow!("普通消息写入不应被忽略"))
    }

    fn write_message_internal_if_new(
        &self,
        message: &NewChatMessage,
    ) -> Result<Option<StoredMessage>> {
        self.write_message_internal_with_conflict_policy(message, true)
    }

    fn write_message_internal_with_conflict_policy(
        &self,
        message: &NewChatMessage,
        ignore_source_message_conflict: bool,
    ) -> Result<Option<StoredMessage>> {
        let now_timestamp = Utc::now().timestamp();
        let mut connection = self.conn_pool.get()?;
        let tx = connection.transaction()?;

        let conversation_id = Self::upsert_conversation(
            &tx,
            &message.source,
            &message.source_conversation_id,
            &message.conversation_kind,
            message.conversation_title.as_deref(),
            &message.conversation_metadata_json,
            message.event_timestamp,
            now_timestamp,
        )?;

        let insert_sql = if ignore_source_message_conflict {
            "
            INSERT INTO messages (
                conversation_id,
                source_message_id,
                sender_id,
                sender_display_name,
                sender_nickname,
                sender_role,
                content_text,
                message_type,
                content_parts_json,
                metadata_json,
                event_timestamp,
                created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            ON CONFLICT(conversation_id, source_message_id) DO NOTHING
            "
        } else {
            "
            INSERT INTO messages (
                conversation_id,
                source_message_id,
                sender_id,
                sender_display_name,
                sender_nickname,
                sender_role,
                content_text,
                message_type,
                content_parts_json,
                metadata_json,
                event_timestamp,
                created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            "
        };
        let changed = tx.execute(
            insert_sql,
            params![
                conversation_id,
                Self::normalize_optional_text(message.source_message_id.as_deref()),
                &message.sender_id,
                &message.sender_display_name,
                Self::normalize_optional_text(message.sender_nickname.as_deref()),
                Self::normalize_optional_text(message.sender_role.as_deref()),
                &message.content_text,
                &message.message_type,
                &message.content_parts_json,
                &message.metadata_json,
                message.event_timestamp,
                now_timestamp,
            ],
        )?;

        if changed == 0 {
            tx.commit()?;
            return Ok(None);
        }

        let stored_message = StoredMessage {
            id: tx.last_insert_rowid(),
        };

        tx.commit()?;
        Ok(Some(stored_message))
    }

    /// 获取指定来源会话的目录记录。
    pub fn get_conversation(
        &self,
        source: &str,
        source_conversation_id: &str,
    ) -> Result<Option<ConversationRecord>> {
        let connection = self.conn_pool.get()?;
        let mut stmt = connection.prepare(
            "
            SELECT
                id,
                source,
                source_conversation_id,
                kind,
                title,
                metadata_json,
                created_at,
                last_message_at
            FROM conversations
            WHERE source = ?1 AND source_conversation_id = ?2
            ",
        )?;

        let mut rows = stmt.query(params![source, source_conversation_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(ConversationRecord::from_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// 使用内部稳定 ID 读取会话，避免管理 API 暴露组合主键规则。
    pub fn get_conversation_by_id(
        &self,
        conversation_id: i64,
    ) -> Result<Option<ConversationRecord>> {
        let connection = self.conn_pool.get()?;
        connection
            .query_row(
                "SELECT id, source, source_conversation_id, kind, title,
                        metadata_json, created_at, last_message_at
                 FROM conversations WHERE id = ?1",
                params![conversation_id],
                ConversationRecord::from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// 按最近活动顺序读取稳定游标分页的会话目录。
    pub fn list_admin_conversations(
        &self,
        kind: Option<&str>,
        cursor: Option<(i64, i64)>,
        limit: u32,
    ) -> Result<Vec<AdminConversationSummary>> {
        let connection = self.conn_pool.get()?;
        let (cursor_time, cursor_id) = cursor.unwrap_or((i64::MAX, i64::MAX));
        let kind = kind.filter(|value| matches!(*value, "group" | "direct"));
        let mut statement = connection.prepare(
            "SELECT c.id, c.source, c.source_conversation_id, c.kind, c.title,
                    c.metadata_json, c.created_at, c.last_message_at,
                    latest.sender_display_name, latest.content_text, latest.event_timestamp,
                    COALESCE(SUM(CASE WHEN m.is_read = 0 THEN 1 ELSE 0 END), 0)
             FROM conversations c
             LEFT JOIN messages latest ON latest.id = (
                 SELECT id FROM messages WHERE conversation_id = c.id ORDER BY id DESC LIMIT 1
             )
             LEFT JOIN messages m ON m.conversation_id = c.id
             WHERE (?1 IS NULL OR c.kind = ?1)
               AND (c.last_message_at < ?2 OR (c.last_message_at = ?2 AND c.id < ?3))
             GROUP BY c.id
             ORDER BY c.last_message_at DESC, c.id DESC
             LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![kind, cursor_time, cursor_id, limit.clamp(1, 100)],
            |row| {
                Ok(AdminConversationSummary {
                    conversation: ConversationRecord {
                        id: row.get(0)?,
                        source: row.get(1)?,
                        source_conversation_id: row.get(2)?,
                        kind: row.get(3)?,
                        title: row.get(4)?,
                        metadata_json: row.get(5)?,
                        created_at: row.get(6)?,
                        last_message_at: row.get(7)?,
                    },
                    latest_sender_name: row.get(8)?,
                    latest_content: row.get(9)?,
                    latest_message_at: row.get(10)?,
                    unread_count: row.get(11)?,
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 读取会话详情中的消息页，返回顺序为从旧到新。
    pub fn get_admin_conversation_messages(
        &self,
        conversation_id: i64,
        before_id: Option<i64>,
        limit: u32,
    ) -> Result<Vec<ChatMessage>> {
        let connection = self.conn_pool.get()?;
        let mut statement = connection.prepare(
            "SELECT m.id, m.conversation_id, c.source, c.source_conversation_id, c.kind,
                    m.source_message_id, m.sender_id, m.sender_display_name, m.sender_nickname,
                    m.sender_role, m.content_text, m.message_type, m.content_parts_json,
                    m.metadata_json, m.is_read, m.event_timestamp, m.created_at
             FROM messages m
             INNER JOIN conversations c ON c.id = m.conversation_id
             WHERE m.conversation_id = ?1 AND (?2 IS NULL OR m.id < ?2)
             ORDER BY m.id DESC LIMIT ?3",
        )?;
        let mut messages = statement
            .query_map(
                params![conversation_id, before_id, limit.clamp(1, 100)],
                ChatMessage::from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        messages.reverse();
        Ok(messages)
    }

    pub fn admin_database_stats(&self, today_start_timestamp: i64) -> Result<AdminDatabaseStats> {
        let connection = self.conn_pool.get()?;
        connection
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM conversations),
                   (SELECT COUNT(*) FROM conversations WHERE kind = 'group'),
                   (SELECT COUNT(*) FROM conversations WHERE kind = 'direct'),
                   (SELECT COUNT(*) FROM messages WHERE event_timestamp >= ?1),
                   (SELECT COUNT(*) FROM user_memories),
                   (SELECT COUNT(*) FROM character_memories),
                   (SELECT COUNT(*) FROM scheduled_tasks)",
                params![today_start_timestamp],
                |row| {
                    Ok(AdminDatabaseStats {
                        conversations: row.get(0)?,
                        group_conversations: row.get(1)?,
                        direct_conversations: row.get(2)?,
                        messages_today: row.get(3)?,
                        user_memories: row.get(4)?,
                        character_memories: row.get(5)?,
                        scheduled_tasks: row.get(6)?,
                    })
                },
            )
            .map_err(Into::into)
    }

    /// 原子清空单个会话的聊天记录，保留会话目录及其定时任务。
    pub fn reset_conversation_history(
        &self,
        source: &str,
        source_conversation_id: &str,
    ) -> Result<ResetConversationResult> {
        let mut connection = self.conn_pool.get()?;
        let tx = connection.transaction()?;
        let conversation_id = tx
            .query_row(
                "
                SELECT id
                FROM conversations
                WHERE source = ?1 AND source_conversation_id = ?2
                ",
                params![source, source_conversation_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let Some(conversation_id) = conversation_id else {
            tx.commit()?;
            return Ok(ResetConversationResult::default());
        };

        let deleted_messages = tx.execute(
            "DELETE FROM messages WHERE conversation_id = ?1",
            params![conversation_id],
        )?;
        tx.execute(
            "UPDATE conversations SET last_message_at = created_at WHERE id = ?1",
            params![conversation_id],
        )?;
        tx.commit()?;

        Ok(ResetConversationResult { deleted_messages })
    }

    /// 获取指定来源会话的聊天记录；按入库顺序分块滚动，平台时间只用于内容展示。
    pub fn get_conversation_history(
        &self,
        source: &str,
        source_conversation_id: &str,
        max_history_messages: u32,
    ) -> Result<Vec<ChatMessage>> {
        Ok(self
            .get_conversation_history_window(source, source_conversation_id, max_history_messages)?
            .messages)
    }

    /// 获取主模型消息窗口，并只在这里计算滚动和合并共同使用的块容量。
    pub fn get_conversation_history_window(
        &self,
        source: &str,
        source_conversation_id: &str,
        max_history_messages: u32,
    ) -> Result<ConversationHistoryWindow> {
        let history_block_size = Self::history_block_size(max_history_messages);
        let messages = self.get_conversation_history_with_block_size(
            source,
            source_conversation_id,
            max_history_messages,
            history_block_size,
        )?;
        Ok(ConversationHistoryWindow {
            messages,
            history_block_size,
        })
    }

    fn get_conversation_history_with_block_size(
        &self,
        source: &str,
        source_conversation_id: &str,
        max_history_messages: u32,
        history_block_size: usize,
    ) -> Result<Vec<ChatMessage>> {
        if max_history_messages == 0 {
            return Ok(Vec::new());
        }

        let connection = self.conn_pool.get()?;
        let total_message_count =
            Self::count_conversation_messages(&connection, source, source_conversation_id)?;
        let history_offset = Self::history_block_offset(
            total_message_count,
            max_history_messages as i64,
            history_block_size as i64,
        );

        let mut stmt = connection.prepare(
            "
            SELECT
                m.id,
                m.conversation_id,
                c.source,
                c.source_conversation_id,
                c.kind,
                m.source_message_id,
                m.sender_id,
                m.sender_display_name,
                m.sender_nickname,
                m.sender_role,
                m.content_text,
                m.message_type,
                m.content_parts_json,
                m.metadata_json,
                m.is_read,
                m.event_timestamp,
                m.created_at
            FROM
                messages m
            INNER JOIN
                conversations c
                ON c.id = m.conversation_id
            WHERE
                c.source = ?1
                AND c.source_conversation_id = ?2
            ORDER BY
                m.id ASC
            LIMIT ?3
            OFFSET ?4
            ",
        )?;

        let messages_iter = stmt.query_map(
            params![
                source,
                source_conversation_id,
                max_history_messages,
                history_offset,
            ],
            |row| ChatMessage::from_row(row),
        )?;

        let mut messages = Vec::new();
        for msg_result in messages_iter {
            messages.push(msg_result?);
        }

        Ok(messages)
    }

    /// 获取会话中最后入库的指定数量消息，并按入库顺序返回。
    pub fn get_latest_conversation_history(
        &self,
        source: &str,
        source_conversation_id: &str,
        limit: u32,
    ) -> Result<Vec<ChatMessage>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let connection = self.conn_pool.get()?;
        let mut stmt = connection.prepare(
            "
            SELECT
                m.id,
                m.conversation_id,
                c.source,
                c.source_conversation_id,
                c.kind,
                m.source_message_id,
                m.sender_id,
                m.sender_display_name,
                m.sender_nickname,
                m.sender_role,
                m.content_text,
                m.message_type,
                m.content_parts_json,
                m.metadata_json,
                m.is_read,
                m.event_timestamp,
                m.created_at
            FROM
                messages m
            INNER JOIN
                conversations c
                ON c.id = m.conversation_id
            WHERE
                c.source = ?1
                AND c.source_conversation_id = ?2
            ORDER BY
                m.id DESC
            LIMIT ?3
            ",
        )?;

        let messages_iter = stmt.query_map(
            params![source, source_conversation_id, limit],
            ChatMessage::from_row,
        )?;
        let mut messages = messages_iter.collect::<rusqlite::Result<Vec<_>>>()?;
        messages.reverse();
        Ok(messages)
    }

    /// 按平台消息 ID 查询同一会话内的原消息，用于还原回复引用。
    pub fn get_referenced_message(
        &self,
        source: &str,
        source_conversation_id: &str,
        source_message_id: &str,
    ) -> Result<Option<ReferencedMessage>> {
        let connection = self.conn_pool.get()?;
        let message = connection
            .query_row(
                "
                SELECT
                    m.sender_display_name,
                    m.content_text
                FROM messages m
                INNER JOIN conversations c ON c.id = m.conversation_id
                WHERE c.source = ?1
                  AND c.source_conversation_id = ?2
                  AND m.source_message_id = ?3
                LIMIT 1
                ",
                params![source, source_conversation_id, source_message_id],
                |row| {
                    Ok(ReferencedMessage {
                        sender_display_name: row.get(0)?,
                        content_text: row.get(1)?,
                    })
                },
            )
            .optional()?;
        Ok(message)
    }

    /// 获取当前会话最新消息的数据库 ID，用作延时检查的起点。
    pub fn get_latest_conversation_message_id(
        &self,
        source: &str,
        source_conversation_id: &str,
    ) -> Result<i64> {
        let connection = self.conn_pool.get()?;
        let message_id = connection.query_row(
            "
            SELECT COALESCE(MAX(m.id), 0)
            FROM messages m
            INNER JOIN conversations c ON c.id = m.conversation_id
            WHERE c.source = ?1 AND c.source_conversation_id = ?2
            ",
            params![source, source_conversation_id],
            |row| row.get(0),
        )?;
        Ok(message_id)
    }

    /// 判断指定账号的发送者是否在给定消息之后发过言。
    pub fn has_sender_message_after(
        &self,
        source: &str,
        source_conversation_id: &str,
        sender_id: &str,
        after_message_id: i64,
    ) -> Result<bool> {
        let connection = self.conn_pool.get()?;
        let exists: i64 = connection.query_row(
            "
            SELECT EXISTS(
                SELECT 1
                FROM messages m
                INNER JOIN conversations c ON c.id = m.conversation_id
                WHERE c.source = ?1
                  AND c.source_conversation_id = ?2
                  AND m.id > ?3
                  AND m.sender_id = ?4
                LIMIT 1
            )
            ",
            params![source, source_conversation_id, after_message_id, sender_id],
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    }

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

    /// 统计指定会话的消息总数，用于计算按块淘汰的窗口位置。
    fn count_conversation_messages(
        connection: &rusqlite::Connection,
        source: &str,
        source_conversation_id: &str,
    ) -> Result<i64> {
        let total_message_count = connection.query_row(
            "
            SELECT COUNT(*)
            FROM
                messages m
            INNER JOIN
                conversations c
                ON c.id = m.conversation_id
            WHERE
                c.source = ?1
                AND c.source_conversation_id = ?2
            ",
            params![source, source_conversation_id],
            |row| row.get(0),
        )?;
        Ok(total_message_count)
    }

    /// 计算一次上下文窗口使用的统一正文块大小。
    fn history_block_size(max_history_messages: u32) -> usize {
        ((max_history_messages as usize) / 5).max(1)
    }

    /// 计算历史窗口需要跳过的旧消息数量，始终淘汰整数个完整正文块。
    fn history_block_offset(
        total_message_count: i64,
        max_history_messages: i64,
        history_block_size: i64,
    ) -> i64 {
        if total_message_count <= max_history_messages {
            return 0;
        }

        let overflow = total_message_count - max_history_messages;
        let dropped_blocks = ((overflow - 1) / history_block_size) + 1;
        dropped_blocks * history_block_size
    }

    /// 将已经加入上下文并成功完成 AI 请求的消息标记为已读。
    pub fn mark_messages_read(&self, message_ids: &[i64]) -> Result<()> {
        if message_ids.is_empty() {
            return Ok(());
        }

        let placeholders = std::iter::repeat("?")
            .take(message_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "UPDATE messages SET is_read = 1 WHERE id IN ({})",
            placeholders
        );
        let connection = self.conn_pool.get()?;
        connection.execute(&sql, params_from_iter(message_ids.iter().copied()))?;
        Ok(())
    }

    /// 根据图片内容哈希查询已接收图片，用于重复图片复用描述。
    pub fn get_received_image_by_hash(
        &self,
        content_hash: &str,
    ) -> Result<Option<ReceivedImageRecord>> {
        let connection = self.conn_pool.get()?;
        let record = connection
            .query_row(
                "
                SELECT image_id, content_hash, local_path, description
                FROM received_images
                WHERE content_hash = ?1
                ",
                params![content_hash],
                |row| {
                    Ok(ReceivedImageRecord {
                        image_id: row.get(0)?,
                        content_hash: row.get(1)?,
                        local_path: row.get(2)?,
                        description: row.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(record)
    }

    /// 根据图片短 ID 查询已接收图片，供后台描述和主模型图像上下文读取原图。
    pub fn get_received_image_by_id(&self, image_id: &str) -> Result<Option<ReceivedImageRecord>> {
        let connection = self.conn_pool.get()?;
        let record = connection
            .query_row(
                "
                SELECT image_id, content_hash, local_path, description
                FROM received_images
                WHERE image_id = ?1
                ",
                params![image_id],
                |row| {
                    Ok(ReceivedImageRecord {
                        image_id: row.get(0)?,
                        content_hash: row.get(1)?,
                        local_path: row.get(2)?,
                        description: row.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(record)
    }

    pub fn get_admin_image_resource(&self, image_id: &str) -> Result<Option<AdminImageResource>> {
        self.conn_pool
            .get()?
            .query_row(
                "SELECT local_path, mime_type FROM received_images WHERE image_id = ?1",
                params![image_id],
                |row| {
                    Ok(AdminImageResource {
                        local_path: row.get(0)?,
                        mime_type: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// 判断图片短 ID 是否已经存在，避免随机 ID 碰撞。
    pub fn received_image_id_exists(&self, image_id: &str) -> Result<bool> {
        let connection = self.conn_pool.get()?;
        let exists: i64 = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM received_images WHERE image_id = ?1)",
            params![image_id],
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    }

    /// 写入一张新接收图片的索引记录。
    pub fn insert_received_image(&self, image: &NewReceivedImage) -> Result<()> {
        let now_timestamp = Utc::now().timestamp();
        let connection = self.conn_pool.get()?;
        connection.execute(
            "
            INSERT INTO received_images (
                image_id,
                content_hash,
                local_path,
                original_url,
                mime_type,
                file_size,
                description,
                metadata_json,
                created_at,
                updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ",
            params![
                &image.image_id,
                &image.content_hash,
                &image.local_path,
                Self::normalize_optional_text(image.original_url.as_deref()),
                Self::normalize_optional_text(image.mime_type.as_deref()),
                image.file_size,
                &image.description,
                &image.metadata_json,
                now_timestamp,
                now_timestamp,
            ],
        )?;
        Ok(())
    }

    /// 完成后台图片描述，并同步更新所有已经引用该图片的消息正文与结构化片段。
    pub fn complete_received_image_description(
        &self,
        image_id: &str,
        description: &str,
        described_text: &str,
    ) -> Result<usize> {
        let now_timestamp = Utc::now().timestamp();
        let mut connection = self.conn_pool.get()?;
        let tx = connection.transaction()?;
        let image_changed = tx.execute(
            "
            UPDATE received_images
            SET description = ?2, updated_at = ?3
            WHERE image_id = ?1
            ",
            params![image_id, description, now_timestamp],
        )?;
        if image_changed == 0 {
            tx.commit()?;
            return Ok(0);
        }

        // 先筛出可能引用该图片的消息，再用 JSON 结构确认并修改对应图片片段。
        let candidates = {
            let mut statement = tx.prepare(
                "
                SELECT id, content_text, content_parts_json
                FROM messages
                WHERE instr(content_parts_json, ?1) > 0
                ",
            )?;
            let rows = statement
                .query_map(params![image_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };

        let placeholder_text = format!("![图片](attachment://{})", image_id);
        let mut updated_messages = 0usize;
        for (message_id, content_text, content_parts_json) in candidates {
            let mut parts: Value = serde_json::from_str(&content_parts_json)?;
            let Some(parts) = parts.as_array_mut() else {
                continue;
            };
            let mut references_image = false;
            let mut context_replacements = Vec::new();
            for part in &mut *parts {
                if part.get("kind").and_then(Value::as_str) != Some("image") {
                    continue;
                }
                let Some(data) = part.get_mut("data").and_then(Value::as_object_mut) else {
                    continue;
                };
                if data.get("image_id").and_then(Value::as_str) != Some(image_id) {
                    continue;
                }
                let previous_context = data
                    .get("context_text")
                    .and_then(Value::as_str)
                    .unwrap_or(&placeholder_text)
                    .to_string();
                let next_context = Self::image_context_with_description(
                    &previous_context,
                    image_id,
                    described_text,
                );
                data.insert(
                    "description".to_string(),
                    Value::String(description.to_string()),
                );
                data.insert(
                    "context_text".to_string(),
                    Value::String(next_context.clone()),
                );
                context_replacements.push((previous_context, next_context));
                references_image = true;
            }
            if !references_image {
                continue;
            }

            let content_text = content_text.map(|mut text| {
                for (previous_context, next_context) in context_replacements {
                    text = text.replace(&previous_context, &next_context);
                }
                text
            });
            let content_parts_json = serde_json::to_string(parts)?;
            tx.execute(
                "
                UPDATE messages
                SET content_text = ?2, content_parts_json = ?3
                WHERE id = ?1
                ",
                params![message_id, content_text, content_parts_json],
            )?;
            updated_messages += 1;
        }

        tx.commit()?;
        Ok(updated_messages)
    }

    /// 只替换内部图片占位，保留文件图片的外层文件信息。
    fn image_context_with_description(
        previous_context: &str,
        image_id: &str,
        described_text: &str,
    ) -> String {
        let image_placeholder = format!("![图片](attachment://{})", image_id);
        if previous_context.contains(&image_placeholder) {
            previous_context.replace(&image_placeholder, described_text)
        } else {
            described_text.to_string()
        }
    }

    /// 查询创建时间早于截止时间的图片文件，供后台资源清理服务处理。
    pub fn get_received_images_created_before(
        &self,
        cutoff_timestamp: i64,
    ) -> Result<Vec<ReceivedImageRecord>> {
        let connection = self.conn_pool.get()?;
        let mut statement = connection.prepare(
            "
            SELECT image_id, content_hash, local_path, description
            FROM received_images
            WHERE created_at < ?1
            ORDER BY created_at ASC, id ASC
            ",
        )?;
        let images = statement
            .query_map(params![cutoff_timestamp], |row| {
                Ok(ReceivedImageRecord {
                    image_id: row.get(0)?,
                    content_hash: row.get(1)?,
                    local_path: row.get(2)?,
                    description: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(images)
    }

    /// 文件清理完成后条件删除图片索引，防止误删截止时间之后创建的记录。
    pub fn delete_received_image_created_before(
        &self,
        image_id: &str,
        cutoff_timestamp: i64,
    ) -> Result<bool> {
        let connection = self.conn_pool.get()?;
        let changed = connection.execute(
            "DELETE FROM received_images WHERE image_id = ?1 AND created_at < ?2",
            params![image_id, cutoff_timestamp],
        )?;
        Ok(changed > 0)
    }

    /// 创建或更新一条会话目录记录，并返回数据库主键。
    fn upsert_conversation(
        tx: &Transaction<'_>,
        source: &str,
        source_conversation_id: &str,
        kind: &str,
        title: Option<&str>,
        metadata_json: &str,
        last_message_at: i64,
        now_timestamp: i64,
    ) -> Result<i64> {
        tx.execute(
            "
            INSERT INTO conversations (
                source,
                source_conversation_id,
                kind,
                title,
                metadata_json,
                created_at,
                last_message_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(source, source_conversation_id) DO UPDATE SET
                kind = excluded.kind,
                title = COALESCE(excluded.title, conversations.title),
                metadata_json = CASE
                    WHEN excluded.metadata_json = '{}' THEN conversations.metadata_json
                    ELSE excluded.metadata_json
                END,
                last_message_at = CASE
                    WHEN excluded.last_message_at > conversations.last_message_at
                    THEN excluded.last_message_at
                    ELSE conversations.last_message_at
                END
            ",
            params![
                source,
                source_conversation_id,
                kind,
                Self::normalize_optional_text(title),
                metadata_json,
                now_timestamp,
                last_message_at,
            ],
        )?;

        let conversation_id = tx.query_row(
            "
            SELECT id
            FROM conversations
            WHERE source = ?1 AND source_conversation_id = ?2
            ",
            params![source, source_conversation_id],
            |row| row.get(0),
        )?;

        Ok(conversation_id)
    }

    /// 将通用会话类型转换为数据库里的字符串值。
    fn conversation_kind_as_str(kind: &ConversationKind) -> &'static str {
        match kind {
            ConversationKind::Direct => "direct",
            ConversationKind::Group => "group",
        }
    }

    /// 过滤掉空字符串，避免把无意义空值写入可选字段。
    fn normalize_optional_text(value: Option<&str>) -> Option<&str> {
        value.and_then(|text| if text.is_empty() { None } else { Some(text) })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{NewChatMessage, NewReceivedImage, QQChatContextManager};
    use rand::Rng;

    // 验证历史窗口按返回的分块大小滚动。
    #[test]
    fn history_window_rolls_by_the_same_block_size_it_returns() {
        let path = temporary_db_path();
        let manager = QQChatContextManager::new(path.to_str().unwrap()).unwrap();
        for index in 1..=11 {
            manager
                .write_message(&test_message(
                    &format!("message-{index}"),
                    &format!("正文 {index}"),
                    index,
                ))
                .unwrap();
        }

        let window = manager
            .get_conversation_history_window("test", "conversation", 10)
            .unwrap();
        assert_eq!(window.history_block_size, 2);
        assert_eq!(window.messages.len(), 9);
        assert_eq!(
            window.messages[0].source_message_id.as_deref(),
            Some("message-3")
        );
        drop(manager);
        fs::remove_file(path).unwrap();
    }

    // 验证重复入库不会新增未读消息。
    #[test]
    fn idempotent_incoming_write_keeps_one_unread_message() {
        let path = temporary_db_path();
        let manager = QQChatContextManager::new(path.to_str().unwrap()).unwrap();
        let message = test_message("message-1", "历史消息", 1);

        let first = manager.write_message_internal_if_new(&message).unwrap();
        let duplicate = manager.write_message_internal_if_new(&message).unwrap();

        assert!(first.is_some());
        assert!(duplicate.is_none());
        let history = manager
            .get_conversation_history("test", "conversation", 10)
            .unwrap();
        assert_eq!(history.len(), 1);
        assert!(!history[0].is_read);

        drop(manager);
        fs::remove_file(path).unwrap();
    }

    // 验证图片描述完成后同步更新图片和消息且保留未读状态。
    #[test]
    fn completed_image_description_updates_image_and_message_without_marking_it_read() {
        let path = temporary_db_path();
        let manager = QQChatContextManager::new(path.to_str().unwrap()).unwrap();
        manager
            .insert_received_image(&NewReceivedImage {
                image_id: "img_A1b2C3d4".to_string(),
                content_hash: "hash".to_string(),
                local_path: "image.jpg".to_string(),
                original_url: None,
                mime_type: Some("image/jpeg".to_string()),
                file_size: 1,
                description: String::new(),
                metadata_json: "{}".to_string(),
            })
            .unwrap();
        let mut message = test_message(
            "message-image",
            "看看 ![图片](attachment://img_A1b2C3d4)",
            1,
        );
        message.message_type = "image".to_string();
        message.content_parts_json = serde_json::json!([{
            "kind": "image",
            "data": {
                "image_id": "img_A1b2C3d4",
                "description": ""
            }
        }])
        .to_string();
        manager.write_message_internal(&message).unwrap();
        let updated = manager
            .complete_received_image_description(
                "img_A1b2C3d4",
                "一张测试图片",
                "![一张测试图片](attachment://img_A1b2C3d4)",
            )
            .unwrap();

        assert_eq!(updated, 1);
        let image = manager
            .get_received_image_by_id("img_A1b2C3d4")
            .unwrap()
            .unwrap();
        assert_eq!(image.description, "一张测试图片");
        let history = manager
            .get_conversation_history("test", "conversation", 10)
            .unwrap();
        assert_eq!(
            history[0].content_text.as_deref(),
            Some("看看 ![一张测试图片](attachment://img_A1b2C3d4)")
        );
        let parts: serde_json::Value =
            serde_json::from_str(&history[0].content_parts_json).unwrap();
        assert_eq!(parts[0]["data"]["description"], "一张测试图片");
        assert!(!history[0].is_read);

        drop(manager);
        fs::remove_file(path).unwrap();
    }

    // 验证重置仅删除当前会话聊天状态。
    #[test]
    fn reset_conversation_removes_only_chat_state_and_keeps_scheduled_tasks() {
        let path = temporary_db_path();
        let manager = QQChatContextManager::new(path.to_str().unwrap()).unwrap();
        manager
            .write_message_internal(&test_message("message-1", "需要删除", 1))
            .unwrap();
        manager
            .insert_scheduled_task(
                "task_ResetKeep",
                "test",
                "conversation",
                "bot",
                "保留任务",
                "cron:0 9 * * *",
                "重置后仍然保留",
                100,
            )
            .unwrap();
        let mut other_message = test_message("other-message", "其它会话", 2);
        other_message.source_conversation_id = "other-conversation".to_string();
        manager.write_message_internal(&other_message).unwrap();

        let result = manager
            .reset_conversation_history("test", "conversation")
            .unwrap();

        assert_eq!(result.deleted_messages, 1);
        assert!(manager
            .get_conversation_history("test", "conversation", 10)
            .unwrap()
            .is_empty());
        assert_eq!(
            manager
                .get_conversation_history("test", "other-conversation", 10)
                .unwrap()
                .len(),
            1
        );
        assert!(manager
            .get_scheduled_task("test", "conversation", "task_ResetKeep")
            .unwrap()
            .is_some());

        drop(manager);
        fs::remove_file(path).unwrap();
    }

    // 验证定时任务按会话隔离、可更新且可原子领取。
    #[test]
    fn scheduled_tasks_are_scoped_updated_and_claimed_atomically() {
        let path = temporary_db_path();
        let manager = QQChatContextManager::new(path.to_str().unwrap()).unwrap();
        manager
            .write_message_internal(&test_message("message-1", "创建会话", 1))
            .unwrap();
        manager
            .insert_scheduled_task(
                "task_A1b2C3d4",
                "test",
                "conversation",
                "bot",
                "测试任务",
                "cron:0 9 * * *",
                "每天检查一次",
                100,
            )
            .unwrap();

        let tasks = manager
            .get_running_scheduled_tasks("test", "conversation")
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "task_A1b2C3d4");
        assert!(manager
            .get_scheduled_task("test", "other-conversation", "task_A1b2C3d4")
            .unwrap()
            .is_none());

        assert!(manager
            .update_scheduled_task(
                "test",
                "conversation",
                "task_A1b2C3d4",
                "新标题",
                "cron:30 10 * * *",
                "新的任务说明",
                200,
            )
            .unwrap());
        assert!(!manager
            .claim_scheduled_task("task_A1b2C3d4", 100, Some(300))
            .unwrap());
        assert!(manager
            .claim_scheduled_task("task_A1b2C3d4", 200, Some(300))
            .unwrap());

        let task = manager
            .get_scheduled_task("test", "conversation", "task_A1b2C3d4")
            .unwrap()
            .unwrap();
        assert_eq!(task.next_run_at, 300);
        assert_eq!(task.last_triggered_at, Some(200));
        assert!(manager
            .claim_scheduled_task("task_A1b2C3d4", 300, None)
            .unwrap());
        assert!(manager
            .get_running_scheduled_tasks("test", "conversation")
            .unwrap()
            .is_empty());

        drop(manager);
        fs::remove_file(path).unwrap();
    }

    fn test_message(source_message_id: &str, content: &str, timestamp: i64) -> NewChatMessage {
        NewChatMessage {
            source: "test".to_string(),
            source_conversation_id: "conversation".to_string(),
            conversation_kind: "group".to_string(),
            conversation_title: None,
            conversation_metadata_json: "{}".to_string(),
            source_message_id: Some(source_message_id.to_string()),
            sender_id: "user".to_string(),
            sender_display_name: "user".to_string(),
            sender_nickname: None,
            sender_role: None,
            content_text: content.to_string(),
            message_type: "text".to_string(),
            content_parts_json: "[]".to_string(),
            metadata_json: "{}".to_string(),
            event_timestamp: timestamp,
        }
    }

    fn temporary_db_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "rolava-context-test-{}-{}.db",
            std::process::id(),
            rand::thread_rng().gen::<u64>()
        ))
    }
}
