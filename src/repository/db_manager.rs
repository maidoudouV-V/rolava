use crate::transport::message::{ConversationKind, IncomingMessage};
use anyhow::{Context, Result};
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
    pub deleted_daily_summaries: usize,
}

/// 主模型本轮使用的数据库消息窗口及其统一分块容量。
#[derive(Debug, Clone)]
pub struct ConversationHistoryWindow {
    pub messages: Vec<ChatMessage>,
    pub history_block_size: usize,
}

/// 一个存在聊天记录但尚未生成摘要的会话统计日。
#[derive(Debug, Clone)]
pub struct PendingDailySummary {
    pub conversation_id: i64,
    pub source: String,
    pub source_conversation_id: String,
    pub summary_date: String,
}

/// 已经生成并可加入主模型上下文的每日摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationDailySummary {
    pub summary_date: String,
    pub summary_text: String,
}

/// 一条待写入的每日聊天摘要。
pub struct NewConversationDailySummary<'a> {
    pub conversation_id: i64,
    pub summary_date: &'a str,
    pub period_start: i64,
    pub period_end: i64,
    pub summary_text: &'a str,
    pub source_message_count: i64,
    pub source_first_message_id: i64,
    pub source_last_message_id: i64,
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
    pub group_memories: u64,
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
    pub sender_nickname: Option<String>,
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

/// 图片的文件管理归属。
#[derive(Debug, Clone, Copy)]
pub enum ImageSource {
    Received,
    Local,
}

impl ImageSource {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Local => "local",
        }
    }
}

/// 已登记图片记录。
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
    pub source: ImageSource,
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
        let mut conn = conn_pool.get()?;

        // 仅迁移旧表名称，保留已有记忆、会话归属和到期状态。
        let has_legacy_memories: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'character_memories')",
            [],
            |row| row.get(0),
        )?;
        if has_legacy_memories {
            conn.execute_batch(
                "BEGIN IMMEDIATE;
                 ALTER TABLE character_memories RENAME TO group_memories;
                 DROP INDEX IF EXISTS idx_character_memories_owner;
                 COMMIT;",
            )?;
        }

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

            CREATE TABLE IF NOT EXISTS conversation_daily_summaries (
                -- 每日摘要的数据库主键。
                id                      INTEGER PRIMARY KEY AUTOINCREMENT,
                -- 摘要所属会话，不同群聊和私聊完全隔离。
                conversation_id         INTEGER NOT NULL,
                -- 以本地时间 00:00 为起点的自然日，格式 YYYY-MM-DD。
                summary_date            TEXT    NOT NULL,
                -- 统计窗口起点的 Unix 时间戳，包含该时刻。
                period_start            INTEGER NOT NULL,
                -- 统计窗口终点的 Unix 时间戳，不包含该时刻。
                period_end              INTEGER NOT NULL,
                -- 主模型生成的当日聊天摘要正文。
                summary_text            TEXT    NOT NULL,
                -- 生成摘要时使用的源消息数量，仅用于查看生成依据。
                source_message_count    INTEGER NOT NULL,
                -- 按事件时间排序后，生成摘要时第一条源消息的数据库 ID。
                source_first_message_id INTEGER NOT NULL,
                -- 按事件时间排序后，生成摘要时最后一条源消息的数据库 ID。
                source_last_message_id  INTEGER NOT NULL,
                -- 摘要成功写入数据库的 Unix 时间戳。
                created_at              INTEGER NOT NULL,
                UNIQUE(conversation_id, summary_date),
                FOREIGN KEY (conversation_id) REFERENCES conversations(id)
            );

            CREATE INDEX IF NOT EXISTS idx_daily_summaries_conversation_date
            ON conversation_daily_summaries (conversation_id, summary_date ASC);

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

            CREATE TABLE IF NOT EXISTS group_memories (
                -- 内部主键用于稳定排序和标记本轮实际展示的记忆。
                id                          INTEGER PRIMARY KEY AUTOINCREMENT,
                -- 平台、机器人账号和会话共同确定一份独立群记忆。
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

            CREATE INDEX IF NOT EXISTS idx_group_memories_owner
            ON group_memories (
                source, bot_id, source_conversation_id, expires_at ASC, id ASC
            );

            CREATE TABLE IF NOT EXISTS received_images (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                image_id        TEXT    NOT NULL UNIQUE,
                content_hash    TEXT    NOT NULL,
                source          TEXT    NOT NULL DEFAULT 'received',
                local_path      TEXT    NOT NULL,
                original_url    TEXT,
                mime_type       TEXT,
                file_size       INTEGER NOT NULL,
                description     TEXT    NOT NULL,
                metadata_json   TEXT    NOT NULL DEFAULT '{}',
                created_at      INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL,
                last_used_at    INTEGER NOT NULL,
                UNIQUE (source, content_hash)
            );

            ",
        )?;
        let has_source: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('received_images') WHERE name = 'source')",
            [], |row| row.get(0),
        )?;
        if !has_source {
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE received_images_new (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    image_id TEXT NOT NULL UNIQUE,
                    content_hash TEXT NOT NULL,
                    source TEXT NOT NULL DEFAULT 'received',
                    local_path TEXT NOT NULL,
                    original_url TEXT,
                    mime_type TEXT,
                    file_size INTEGER NOT NULL,
                    description TEXT NOT NULL,
                    metadata_json TEXT NOT NULL DEFAULT '{}',
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    last_used_at INTEGER NOT NULL,
                    UNIQUE (source, content_hash)
                );
                INSERT INTO received_images_new
                SELECT id, image_id,
                    CASE WHEN content_hash LIKE 'local:%' THEN substr(content_hash, 7) ELSE content_hash END,
                    CASE WHEN content_hash LIKE 'local:%' THEN 'local' ELSE 'received' END,
                    local_path, original_url, mime_type, file_size, description, metadata_json,
                    created_at, updated_at, updated_at
                FROM received_images;
                DROP TABLE received_images;
                ALTER TABLE received_images_new RENAME TO received_images;",
            )?;
            tx.commit()?;
        }
        conn.execute_batch(
            "DROP INDEX IF EXISTS idx_received_images_content_hash;
             CREATE INDEX IF NOT EXISTS idx_received_images_last_used
             ON received_images(source, last_used_at);",
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
                    COALESCE(NULLIF(TRIM(latest.sender_nickname), ''), latest.sender_display_name),
                    latest.content_text, latest.event_timestamp,
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
                   (SELECT COUNT(*) FROM group_memories),
                   (SELECT COUNT(*) FROM scheduled_tasks)",
                params![today_start_timestamp],
                |row| {
                    Ok(AdminDatabaseStats {
                        conversations: row.get(0)?,
                        group_conversations: row.get(1)?,
                        direct_conversations: row.get(2)?,
                        messages_today: row.get(3)?,
                        user_memories: row.get(4)?,
                        group_memories: row.get(5)?,
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
        let deleted_daily_summaries = tx.execute(
            "DELETE FROM conversation_daily_summaries WHERE conversation_id = ?1",
            params![conversation_id],
        )?;
        tx.execute(
            "UPDATE conversations SET last_message_at = created_at WHERE id = ?1",
            params![conversation_id],
        )?;
        tx.commit()?;

        Ok(ResetConversationResult {
            deleted_messages,
            deleted_daily_summaries,
        })
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

    /// 查找已经结束且存在消息、但尚未生成摘要的会话统计日。
    pub fn get_pending_daily_summaries(
        &self,
        closed_before_timestamp: i64,
    ) -> Result<Vec<PendingDailySummary>> {
        let connection = self.conn_pool.get()?;
        let mut statement = connection.prepare(
            "
            WITH daily_messages AS (
                SELECT
                    conversation_id,
                    date(event_timestamp, 'unixepoch', 'localtime') AS summary_date
                FROM messages
                WHERE event_timestamp < ?1
                GROUP BY conversation_id, summary_date
            )
            SELECT
                d.conversation_id,
                c.source,
                c.source_conversation_id,
                d.summary_date
            FROM daily_messages d
            INNER JOIN conversations c ON c.id = d.conversation_id
            LEFT JOIN conversation_daily_summaries s
                ON s.conversation_id = d.conversation_id
                AND s.summary_date = d.summary_date
            WHERE s.id IS NULL
            ORDER BY d.summary_date ASC, d.conversation_id ASC
            ",
        )?;
        let summaries = statement
            .query_map(params![closed_before_timestamp], |row| {
                Ok(PendingDailySummary {
                    conversation_id: row.get(0)?,
                    source: row.get(1)?,
                    source_conversation_id: row.get(2)?,
                    summary_date: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(summaries)
    }

    /// 只生成原始消息窗口最早日期及之前的缺失摘要，每个会话共用一次窗口查询。
    pub fn get_pending_window_daily_summaries(
        &self,
        closed_before_timestamp: i64,
        group_max_history_messages: u32,
        direct_max_history_messages: u32,
    ) -> Result<Vec<PendingDailySummary>> {
        let mut boundaries = std::collections::HashMap::new();
        let mut pending = Vec::new();
        for task in self.get_pending_daily_summaries(closed_before_timestamp)? {
            if let std::collections::hash_map::Entry::Vacant(entry) = boundaries.entry(task.conversation_id) {
                let conversation = self.get_conversation_by_id(task.conversation_id)?
                    .context("摘要会话不存在")?;
                let limit = match conversation.kind.as_str() {
                    "group" => group_max_history_messages,
                    "direct" => direct_max_history_messages,
                    kind => anyhow::bail!("未知会话类型：{}", kind),
                };
                let window = self.get_conversation_history_window(
                    &task.source, &task.source_conversation_id, limit,
                )?;
                let boundary = window.messages.first()
                    .map(|message| crate::history_compression::summary_date_for_timestamp(message.event_timestamp))
                    .transpose()?;
                entry.insert(boundary);
            }
            if boundaries[&task.conversation_id].as_ref()
                .is_some_and(|boundary| task.summary_date.as_str() <= boundary.as_str()) {
                pending.push(task);
            }
        }
        Ok(pending)
    }

    /// 读取一个会话统计日的全部消息，并按平台时间和数据库 ID 排序。
    pub fn get_daily_summary_source_messages(
        &self,
        conversation_id: i64,
        summary_date: &str,
    ) -> Result<Vec<ChatMessage>> {
        let (start, end) = crate::history_compression::summary_period(summary_date)?;
        self.get_summary_messages_in_period(conversation_id, start.timestamp(), end.timestamp())
    }

    /// 读取摘要正文或参考时段的消息，包含起点、不包含终点。
    pub fn get_summary_messages_in_period(
        &self,
        conversation_id: i64,
        start_timestamp: i64,
        end_timestamp: i64,
    ) -> Result<Vec<ChatMessage>> {
        let connection = self.conn_pool.get()?;
        let mut statement = connection.prepare(
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
            FROM messages m
            INNER JOIN conversations c ON c.id = m.conversation_id
            WHERE m.conversation_id = ?1
              AND m.event_timestamp >= ?2 AND m.event_timestamp < ?3
            ORDER BY m.event_timestamp ASC, m.id ASC
            ",
        )?;
        let messages = statement
            .query_map(
                params![conversation_id, start_timestamp, end_timestamp],
                ChatMessage::from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(messages)
    }

    /// 幂等写入每日摘要；已有同会话同日期记录时保持原内容不变。
    pub fn insert_conversation_daily_summary(
        &self,
        summary: &NewConversationDailySummary<'_>,
    ) -> Result<bool> {
        let connection = self.conn_pool.get()?;
        let inserted = connection.execute(
            "
            INSERT OR IGNORE INTO conversation_daily_summaries (
                conversation_id,
                summary_date,
                period_start,
                period_end,
                summary_text,
                source_message_count,
                source_first_message_id,
                source_last_message_id,
                created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ",
            params![
                summary.conversation_id,
                summary.summary_date,
                summary.period_start,
                summary.period_end,
                summary.summary_text,
                summary.source_message_count,
                summary.source_first_message_id,
                summary.source_last_message_id,
                Utc::now().timestamp(),
            ],
        )?;
        Ok(inserted > 0)
    }

    /// 按包含两端的统计日期范围读取摘要，旧摘要仍保留在数据库中。
    pub fn get_conversation_daily_summaries_between(
        &self,
        conversation_id: i64,
        from_summary_date: &str,
        through_summary_date: &str,
    ) -> Result<Vec<ConversationDailySummary>> {
        let connection = self.conn_pool.get()?;
        let mut statement = connection.prepare(
            "
            SELECT summary_date, summary_text
            FROM conversation_daily_summaries
            WHERE conversation_id = ?1 AND summary_date BETWEEN ?2 AND ?3
            ORDER BY summary_date ASC, id ASC
            ",
        )?;
        let summaries = statement
            .query_map(
                params![conversation_id, from_summary_date, through_summary_date],
                |row| {
                    Ok(ConversationDailySummary {
                        summary_date: row.get(0)?,
                        summary_text: row.get(1)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(summaries)
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
                    m.sender_nickname,
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
                        sender_nickname: row.get(1)?,
                        content_text: row.get(2)?,
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
    pub(super) fn normalize_optional_text(value: Option<&str>) -> Option<&str> {
        value.and_then(|text| if text.is_empty() { None } else { Some(text) })
    }
}
