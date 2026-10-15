use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::RwLock;
use tokio_util::sync::CancellationToken;

use super::log_buffer::AdminLogBuffer;
use crate::config::AppConfig;
use crate::memory::UserMemoryService;
use crate::model_metadata::ModelMetadataStore;
use crate::repository::db_manager::QQChatContextManager;
use crate::request_statistics::RequestStatisticsStore;
use crate::runtime_state::RuntimeState;
use crate::scheduler::SchedulerService;
use crate::transport::onebot::OneBotHttpServer;

#[derive(Clone)]
pub struct AdminState {
    app_config: Arc<RwLock<Arc<AppConfig>>>,
    // 与运行中的主模型一致；仅保存配置但未重启时，不提前切换窗口大小。
    pub(super) group_context_history_limit: u32,
    pub(super) direct_context_history_limit: u32,
    pub(super) context_summary_days: u16,
    // 使用启动时的值，确保页面与运行中的模型上下文一致。
    pub(super) context_summary_enabled: bool,
    pub config_path: PathBuf,
    pub db_manager: Arc<QQChatContextManager>,
    pub scheduler: Arc<SchedulerService>,
    pub onebot: Arc<OneBotHttpServer>,
    pub runtime: Arc<RuntimeState>,
    pub logs: Arc<AdminLogBuffer>,
    pub restart: CancellationToken,
    pub started_at: i64,
    pub(super) user_memory: Arc<UserMemoryService>,
    pub(super) model_metadata: Arc<ModelMetadataStore>,
    pub request_statistics: Arc<RequestStatisticsStore>,
}

impl AdminState {
    pub fn new(
        app_config: Arc<AppConfig>,
        config_path: PathBuf,
        db_manager: Arc<QQChatContextManager>,
        scheduler: Arc<SchedulerService>,
        onebot: Arc<OneBotHttpServer>,
        runtime: Arc<RuntimeState>,
        logs: Arc<AdminLogBuffer>,
        restart: CancellationToken,
        model_metadata: Arc<ModelMetadataStore>,
        request_statistics: Arc<RequestStatisticsStore>,
    ) -> Self {
        Self {
            request_statistics,
            group_context_history_limit: app_config.app.group_max_history_messages,
            direct_context_history_limit: app_config.app.direct_max_history_messages,
            context_summary_days: app_config.app.history_summary_days,
            context_summary_enabled: app_config.app.history_summary_enabled,
            user_memory: Arc::new(UserMemoryService::new(db_manager.clone())),
            app_config: Arc::new(RwLock::new(app_config)),
            config_path,
            db_manager,
            scheduler,
            onebot,
            runtime,
            logs,
            restart,
            model_metadata,
            started_at: Utc::now().timestamp(),
        }
    }

    /// 管理后台使用可替换快照，普通保存后刷新页面也能读取最新配置。
    pub(super) fn app_config(&self) -> Arc<AppConfig> {
        self.app_config.read().clone()
    }

    pub(super) fn replace_app_config(&self, config: AppConfig) {
        *self.app_config.write() = Arc::new(config);
    }
}
