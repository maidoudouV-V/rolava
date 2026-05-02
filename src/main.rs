mod ai_provider;
mod repository;
mod config;
mod message_enricher;
mod pipeline;
mod transport;

use crate::ai_provider::ChatResponse;
use crate::config::{AppConfig};
use pipeline::chat_pipeline::ChatPipeline;
use repository::db_manager::QQChatContextManager;
use std::sync::Arc;
use tokio::select;
use transport::onebot::OneBotHttpServer;

#[tokio::main]
async fn main() {
    // 读取所有配置
    let app_config = AppConfig::new("config/meta.toml").expect("配置文件读取失败");

    // 接收QQ消息的HTTP服务
    let qq_receive_server = Arc::new(OneBotHttpServer::new(&app_config));

    // 测试数据库
    let manager = QQChatContextManager::new("test_chat.db").unwrap();
    let db_manager = Arc::new(manager);
    // 消息处理流程
    let mut chat_pipeline = ChatPipeline::new(db_manager.clone(), qq_receive_server.clone(), app_config);
    
    // 运行所有服务
    select! {
        _ = qq_receive_server.run() => {
            println!("HTTP server stopped.");
        }
        _ = chat_pipeline.start_process_chat() => {
            println!("Processing messages stopped.");
        }
    }
}

pub struct SendMessageInfo {
    pub group_id: i64,
    pub bot_id: i64,
    pub bot_nickname: String,
    pub message: ChatResponse,
}
impl SendMessageInfo {
    pub fn new(group_id: i64, bot_id: i64, bot_nickname: String, message: ChatResponse) -> Self{
        Self{
            group_id,
            bot_id,
            bot_nickname,
            message,
        }
    }
}

