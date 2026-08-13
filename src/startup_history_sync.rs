use std::sync::Arc;

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::config::AppConfig;
use crate::message_ingestion::MessageIngestionService;
use crate::repository::db_manager::QQChatContextManager;
use crate::transport::message::IncomingMessage;
use crate::transport::onebot::OneBotHttpServer;

const STARTUP_GROUP_HISTORY_COUNT: u32 = 99;

/// 程序启动时回填群历史；只补数据库，不进入过滤器、Actor 或主模型流程。
pub struct StartupHistorySyncService {
    app_config: Arc<AppConfig>,
    db_manager: Arc<QQChatContextManager>,
    message_ingestion: Arc<MessageIngestionService>,
    onebot: Arc<OneBotHttpServer>,
}

impl StartupHistorySyncService {
    pub fn new(
        app_config: Arc<AppConfig>,
        db_manager: Arc<QQChatContextManager>,
        onebot: Arc<OneBotHttpServer>,
        message_ingestion: Arc<MessageIngestionService>,
    ) -> Self {
        Self {
            app_config: app_config.clone(),
            db_manager,
            message_ingestion,
            onebot,
        }
    }

    /// 同步失败不能阻止机器人启动，具体失败原因记录到日志。
    pub async fn run(&self, history_before_timestamp: i64) {
        if let Err(error) = self.sync(history_before_timestamp).await {
            warn!(error = %error, "启动群历史同步失败，继续启动其它服务");
        }
    }

    async fn sync(&self, history_before_timestamp: i64) -> Result<()> {
        let bot_id = self.onebot.fetch_login_user_id().await?;
        let group_ids = self.target_group_ids().await?;
        let mut inserted_count = 0usize;

        info!(group_count = group_ids.len(), "开始同步启动群历史");
        for group_id in group_ids {
            match self
                .sync_group(group_id, bot_id, history_before_timestamp)
                .await
            {
                Ok(group_inserted_count) => inserted_count += group_inserted_count,
                Err(error) => {
                    warn!(group_id, error = %error, "同步单个群历史失败");
                }
            }
        }
        info!(inserted_count, "启动群历史同步完成");
        Ok(())
    }

    async fn target_group_ids(&self) -> Result<Vec<i64>> {
        if self.app_config.app.group_whitelist.is_empty() {
            return self.onebot.fetch_group_ids().await;
        }

        let mut group_ids = Vec::new();
        for configured_id in &self.app_config.app.group_whitelist {
            match configured_id.trim().parse::<i64>() {
                Ok(group_id) => group_ids.push(group_id),
                Err(error) => {
                    warn!(group_id = %configured_id, error = %error, "跳过无效的群白名单 ID");
                }
            }
        }
        group_ids.sort_unstable();
        group_ids.dedup();
        Ok(group_ids)
    }

    async fn sync_group(
        &self,
        group_id: i64,
        bot_id: i64,
        history_before_timestamp: i64,
    ) -> Result<usize> {
        let history = self
            .onebot
            .fetch_group_history(group_id, bot_id, STARTUP_GROUP_HISTORY_COUNT)
            .await?;
        // 接收器启动后的消息留给实时 Actor，避免回填去重使它们失去触发 AI 的机会。
        let history = history
            .into_iter()
            .filter(|message| message.timestamp < history_before_timestamp)
            .collect();
        let history = Self::messages_after_last_bot(history, &bot_id.to_string());
        let history = self.messages_after_last_existing(history)?;
        let candidate_count = history.len();
        let mut inserted_count = 0usize;

        for message in history {
            if self.message_ingestion.ingest(message).await?.is_some() {
                inserted_count += 1;
            }
        }
        debug!(
            group_id,
            candidate_count, inserted_count, "单个群历史同步完成"
        );
        Ok(inserted_count)
    }

    /// 当前机器人最后一次发言是恢复边界，机器人消息本身永远不会进入历史回填。
    fn messages_after_last_bot(
        messages: Vec<IncomingMessage>,
        bot_id: &str,
    ) -> Vec<IncomingMessage> {
        let start_index = messages
            .iter()
            .rposition(|message| message.sender.id == bot_id)
            .map_or(0, |index| index + 1);
        messages
            .into_iter()
            .skip(start_index)
            .filter(|message| message.sender.id != bot_id)
            .collect()
    }

    /// 已入库消息组成连续前缀时只追加后续内容，逐条入库仍保留最终幂等保护。
    fn messages_after_last_existing(
        &self,
        messages: Vec<IncomingMessage>,
    ) -> Result<Vec<IncomingMessage>> {
        let mut start_index = 0;
        for (index, message) in messages.iter().enumerate() {
            if self.db_manager.incoming_message_exists(message)? {
                start_index = index + 1;
            }
        }
        Ok(messages.into_iter().skip(start_index).collect())
    }
}
