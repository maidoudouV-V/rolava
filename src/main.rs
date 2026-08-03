mod ai_provider;
mod config;
mod message_enricher;
mod pipeline;
mod repository;
pub mod tools;
mod transport;

use crate::config::AppConfig;
use pipeline::dispatcher::ConversationDispatcher;
use repository::db_manager::QQChatContextManager;
use std::sync::Arc;
use tokio::{select, sync::mpsc};
use transport::message::IncomingMessage;
use transport::onebot::{OneBotHttpServer, OneBotMessageSender};
use transport::MessageSender;

const MESSAGE_CHANNEL_CAPACITY: usize = 128;

#[tokio::main]
async fn main() {
    // 读取所有配置
    let app_config = Arc::new(AppConfig::new("config/meta.toml").expect("配置文件读取失败"));

    let (platform_tx, platform_rx) = mpsc::channel::<IncomingMessage>(MESSAGE_CHANNEL_CAPACITY);

    // 测试数据库
    let manager = QQChatContextManager::new("test_chat.db").unwrap();
    let db_manager = Arc::new(manager);

    // OneBot 入站服务与出站发送器分离；发送器只通过通用接口交给会话流程。
    let qq_receive_server = Arc::new(OneBotHttpServer::new(app_config.as_ref(), platform_tx));
    let message_sender: Arc<dyn MessageSender> = Arc::new(OneBotMessageSender::new(
        app_config.as_ref(),
        db_manager.clone(),
    ));

    // 平台消息按会话投递给独立 Actor。
    let mut conversation_dispatcher = ConversationDispatcher::new(
        app_config.clone(),
        db_manager.clone(),
        message_sender,
        platform_rx,
    );

    // 运行所有服务
    select! {
        _ = qq_receive_server.run() => {
            println!("HTTP server stopped.");
        }
        _ = conversation_dispatcher.run() => {
            println!("Conversation dispatcher stopped.");
        }
    }
}
