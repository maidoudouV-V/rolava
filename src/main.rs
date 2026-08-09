mod ai_provider;
mod commands;
mod config;
mod conversation_context;
pub mod conversation_control;
pub mod conversation_trigger;
mod memory;
mod message_enricher;
mod message_ingestion;
mod pipeline;
mod repository;
mod resource_cleanup;
mod scheduler;
mod startup_history_sync;
pub mod tools;
mod transport;

use crate::config::AppConfig;
use crate::message_ingestion::MessageIngestionService;
use chrono::Utc;
use pipeline::dispatcher::ConversationDispatcher;
use repository::db_manager::QQChatContextManager;
use resource_cleanup::ResourceCleanupService;
use scheduler::SchedulerService;
use startup_history_sync::StartupHistorySyncService;
use std::sync::Arc;
use tokio::{
    select,
    sync::{mpsc, oneshot},
};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use transport::message::IncomingMessage;
use transport::onebot::{OneBotHttpServer, OneBotMessageSender};
use transport::MessageSender;

const MESSAGE_CHANNEL_CAPACITY: usize = 128;

#[tokio::main]
async fn main() {
    // 读取所有配置
    let app_config = Arc::new(AppConfig::new("config/meta.toml").expect("配置文件读取失败"));
    init_tracing(app_config.logging.level.as_str());

    let (platform_tx, platform_rx) = mpsc::channel::<IncomingMessage>(MESSAGE_CHANNEL_CAPACITY);
    let (internal_trigger_tx, internal_trigger_rx) = mpsc::unbounded_channel();

    // 测试数据库
    let manager = QQChatContextManager::new("test_chat.db").unwrap();
    let db_manager = Arc::new(manager);
    let message_ingestion = Arc::new(MessageIngestionService::new(
        app_config.clone(),
        db_manager.clone(),
    ));
    let scheduler = Arc::new(SchedulerService::new(
        db_manager.clone(),
        internal_trigger_tx.clone(),
    ));
    let resource_cleanup = Arc::new(ResourceCleanupService::new(
        db_manager.clone(),
        &app_config.app.received_image_dir,
    ));

    // OneBot 入站服务与出站发送器分离；发送器只通过通用接口交给会话流程。
    let qq_receive_server = Arc::new(OneBotHttpServer::new(
        app_config.as_ref(),
        platform_tx,
        internal_trigger_tx,
    ));
    // 先启动上报接收器并缓存实时消息，再回填启动时刻之前的历史记录。
    let (receive_ready_tx, receive_ready_rx) = oneshot::channel();
    let receive_server = qq_receive_server.clone();
    let qq_receive_task = tokio::spawn(async move {
        receive_server.run(receive_ready_tx).await;
    });
    if receive_ready_rx.await.is_err() {
        warn!("OneBot HTTP 服务未能完成启动通知");
    }
    let history_before_timestamp = Utc::now().timestamp();
    StartupHistorySyncService::new(
        app_config.clone(),
        db_manager.clone(),
        qq_receive_server.clone(),
        message_ingestion.clone(),
    )
    .run(history_before_timestamp)
    .await;
    let message_sender: Arc<dyn MessageSender> = Arc::new(OneBotMessageSender::new(
        app_config.as_ref(),
        db_manager.clone(),
    ));

    // 平台消息按会话投递给独立 Actor。
    let mut conversation_dispatcher = ConversationDispatcher::new(
        app_config.clone(),
        db_manager.clone(),
        message_sender,
        scheduler.clone(),
        message_ingestion,
        platform_rx,
        internal_trigger_rx,
    );

    // 运行所有服务
    info!("服务启动完成");
    select! {
        _ = qq_receive_task => {
            warn!("HTTP 服务已停止");
        }
        _ = conversation_dispatcher.run() => {
            warn!("会话分发器已停止");
        }
        _ = scheduler.run() => {
            warn!("定时任务调度器已停止");
        }
        _ = resource_cleanup.run() => {
            warn!("资源清理服务已停止");
        }
    }
}

fn init_tracing(configured_level: &str) {
    let default_filter = format!("warn,rolava={}", configured_level);
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .compact()
        .init();
}
