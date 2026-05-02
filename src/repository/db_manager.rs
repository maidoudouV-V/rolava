use anyhow::Result;
use chrono::Utc;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, params_from_iter, OptionalExtension, Row, Transaction};
use serde_json::{json, Value};
use crate::transport::message::{ConversationKind, IncomingMessage};

/// 一条会话目录记录。
#[derive(Debug, Clone)]
pub struct ConversationRecord {
    /// 会话表主键。
    pub id: i64,
    /// 消息来源平台，例如 `onebot`。
    pub source: String,
    /// 来源平台上的会话 ID。
    pub source_conversation_id: String,
    /// 会话类型，例如 `direct` / `group` / `channel`。
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
    conn_pool: Pool<SqliteConnectionManager>,
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

            CREATE INDEX IF NOT EXISTS idx_messages_conversation_timestamp
            ON messages (conversation_id, event_timestamp DESC, id DESC);

            CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_conversation_source_message
            ON messages (conversation_id, source_message_id);

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
            "
        )?;
        Self::ensure_messages_is_read_column(&conn)?;

        Ok(Self { conn_pool })
    }

    /// 兼容旧数据库：如果 messages 表还没有 is_read 字段，则追加字段并给旧数据默认未读。
    fn ensure_messages_is_read_column(conn: &rusqlite::Connection) -> Result<()> {
        let mut stmt = conn.prepare("PRAGMA table_info(messages)")?;
        let column_names = stmt.query_map([], |row| row.get::<_, String>(1))?;
        for column_name in column_names {
            if column_name? == "is_read" {
                return Ok(());
            }
        }

        conn.execute(
            "ALTER TABLE messages ADD COLUMN is_read INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        Ok(())
    }

    /// 将通用入站消息转换为标准写入请求。
    pub fn new_message_from_incoming(message: &IncomingMessage) -> NewChatMessage {
        let message_type = message.content.parts
            .first()
            .map(|part| part.kind.clone())
            .unwrap_or_else(|| "text".to_string());
        let content_parts_json = Value::Array(
            message.content.parts
                .iter()
                .map(|part| {
                    json!({
                        "kind": part.kind,
                        "data": part.data
                    })
                })
                .collect()
        ).to_string();

        NewChatMessage {
            source: message.source.clone(),
            source_conversation_id: message.conversation.id.clone(),
            conversation_kind: Self::conversation_kind_as_str(&message.conversation.kind).to_string(),
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
    pub fn write_incoming_message(&self, message: &IncomingMessage) -> Result<()> {
        let new_message = Self::new_message_from_incoming(message);
        self.write_message(&new_message)
    }

    /// 写入一条标准化后的聊天记录。
    pub fn write_message(&self, message: &NewChatMessage) -> Result<()> {
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

        tx.execute(
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
            ",
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

        tx.commit()?;
        Ok(())
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
            "
        )?;

        let mut rows = stmt.query(params![source, source_conversation_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(ConversationRecord::from_row(row)?))
        } else {
            Ok(None)
        }
    }

    /// 获取指定来源会话的聊天记录；超过最大数量后按块淘汰旧消息，避免每条新消息都移动窗口。
    pub fn get_conversation_history(
        &self,
        source: &str,
        source_conversation_id: &str,
        max_history_messages: u32,
    ) -> Result<Vec<ChatMessage>> {
        if max_history_messages == 0 {
            return Ok(Vec::new());
        }

        let connection = self.conn_pool.get()?;
        let total_message_count = Self::count_conversation_messages(
            &connection,
            source,
            source_conversation_id,
        )?;
        let history_offset = Self::history_block_offset(
            total_message_count,
            max_history_messages as i64,
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
                m.event_timestamp ASC,
                m.id ASC
            LIMIT ?3
            OFFSET ?4
            "
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

    /// 计算历史窗口需要跳过的旧消息数量；块大小为最大历史消息数的十分之一。
    fn history_block_offset(total_message_count: i64, max_history_messages: i64) -> i64 {
        if total_message_count <= max_history_messages {
            return 0;
        }

        let block_size = (max_history_messages / 10).max(1);
        let overflow = total_message_count - max_history_messages;
        let dropped_blocks = ((overflow - 1) / block_size) + 1;
        dropped_blocks * block_size
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
    pub fn get_received_image_by_hash(&self, content_hash: &str) -> Result<Option<ReceivedImageRecord>> {
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
            ConversationKind::Channel => "channel",
        }
    }

    /// 过滤掉空字符串，避免把无意义空值写入可选字段。
    fn normalize_optional_text(value: Option<&str>) -> Option<&str> {
        value.and_then(|text| {
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        })
    }
}
