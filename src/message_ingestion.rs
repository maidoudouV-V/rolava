use std::sync::Arc;

use anyhow::Result;

use crate::config::AppConfig;
use crate::message_enricher::MessageEnricher;
use crate::repository::db_manager::{QQChatContextManager, StoredMessage};
use crate::transport::message::IncomingMessage;

/// 一条已经完成统一多媒体增强并写入数据库的平台消息。
pub struct IngestedMessage {
    pub message: IncomingMessage,
    pub stored: StoredMessage,
}

/// 平台消息的共享入库入口；实时消息和历史回填必须经过同一套增强逻辑。
pub struct MessageIngestionService {
    db_manager: Arc<QQChatContextManager>,
    message_enricher: MessageEnricher,
}

impl MessageIngestionService {
    pub fn new(app_config: Arc<AppConfig>, db_manager: Arc<QQChatContextManager>) -> Self {
        Self {
            db_manager: db_manager.clone(),
            message_enricher: MessageEnricher::new(app_config, db_manager),
        }
    }

    /// 平台消息 ID 已存在时直接跳过，避免重复执行图片下载和视觉识别。
    pub async fn ingest(&self, message: IncomingMessage) -> Result<Option<IngestedMessage>> {
        if self.db_manager.incoming_message_exists(&message)? {
            return Ok(None);
        }

        let enriched = self.message_enricher.enrich(message).await;
        let message = &enriched.message;
        let Some(stored) = self.db_manager.write_incoming_message_if_new(message)? else {
            return Ok(None);
        };
        self.message_enricher
            .schedule_pending_descriptions(&enriched);

        Ok(Some(IngestedMessage {
            message: enriched.message,
            stored,
        }))
    }
}
