mod admin;
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
mod runtime_state;
mod scheduler;
mod startup_history_sync;
pub mod tools;
mod transport;

use crate::admin::AdminState;
use crate::config::AppConfig;
use crate::message_ingestion::MessageIngestionService;
use anyhow::{Context, Result};
use chrono::Utc;
use pipeline::dispatcher::ConversationDispatcher;
use repository::db_manager::QQChatContextManager;
use resource_cleanup::ResourceCleanupService;
use runtime_state::RuntimeState;
use scheduler::SchedulerService;
use startup_history_sync::StartupHistorySyncService;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};
use transport::message::IncomingMessage;
use transport::onebot::{OneBotHttpServer, OneBotMessageSender};
use transport::MessageSender;

const MESSAGE_CHANNEL_CAPACITY: usize = 128;
const WORKER_ENV: &str = "ROLAVA_WORKER";
const RESTART_EXIT_CODE: i32 = 75;

#[tokio::main]
async fn main() {
    let exit_code = if std::env::var_os(WORKER_ENV).is_some() {
        match run_worker().await {
            Ok(WorkerExit::Restart) => RESTART_EXIT_CODE,
            Ok(WorkerExit::Stop) => 0,
            Err(error) => {
                eprintln!("Rolava worker 启动失败：{:#}", error);
                1
            }
        }
    } else {
        match run_supervisor().await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("Rolava supervisor 运行失败：{:#}", error);
                1
            }
        }
    };
    std::process::exit(exit_code);
}

enum WorkerExit {
    Restart,
    Stop,
}

/// 父进程只处理受控重启和系统退出，不参与机器人业务。
async fn run_supervisor() -> Result<i32> {
    loop {
        let executable = std::env::current_exe().context("获取当前程序路径失败")?;
        let mut child = Command::new(executable)
            .env(WORKER_ENV, "1")
            .spawn()
            .context("启动 Rolava worker 失败")?;

        let status = tokio::select! {
            status = child.wait() => status.context("等待 Rolava worker 退出失败")?,
            _ = shutdown_signal() => {
                terminate_child(&mut child).await;
                return Ok(0);
            }
        };
        if status.code() == Some(RESTART_EXIT_CODE) {
            continue;
        }
        return Ok(exit_code(status));
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

async fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // Docker 将 SIGTERM 发给 PID 1 时，supervisor 转发给业务 worker。
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    let _ = child.start_kill();

    if tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate()).expect("注册 SIGTERM 失败");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.expect("注册 Ctrl+C 失败");
}

async fn run_worker() -> Result<WorkerExit> {
    let config_path = admin::config_path();
    admin::ensure_admin_token(&config_path)?;
    let app_config = Arc::new(
        AppConfig::new(config_path.to_string_lossy().as_ref()).context("配置文件读取失败")?,
    );
    let admin_logs = Arc::new(admin::AdminLogBuffer::new(1000));
    init_tracing(app_config.logging.level.as_str(), admin_logs.clone());

    let restart = CancellationToken::new();
    let runtime = Arc::new(RuntimeState::default());
    let (platform_tx, platform_rx) = mpsc::channel::<IncomingMessage>(MESSAGE_CHANNEL_CAPACITY);
    let (internal_trigger_tx, internal_trigger_rx) = mpsc::unbounded_channel();
    let db_manager = Arc::new(QQChatContextManager::new("test_chat.db")?);
    let message_ingestion = Arc::new(MessageIngestionService::new(
        app_config.clone(),
        db_manager.clone(),
    ));
    let scheduler = Arc::new(SchedulerService::new(
        db_manager.clone(),
        internal_trigger_tx.clone(),
        app_config.prompt_config.scheduled_task_prompt.clone(),
        app_config
            .prompt_config
            .scheduled_task_recovery_prompt
            .clone(),
    ));
    let resource_cleanup = Arc::new(ResourceCleanupService::new(
        db_manager.clone(),
        &app_config.app.received_image_dir,
    ));

    let qq_receive_server = Arc::new(OneBotHttpServer::new(
        app_config.as_ref(),
        platform_tx,
        internal_trigger_tx,
        runtime.clone(),
    ));
    let admin_state = Arc::new(AdminState::new(
        app_config.clone(),
        config_path,
        db_manager.clone(),
        scheduler.clone(),
        qq_receive_server.clone(),
        runtime.clone(),
        admin_logs,
        restart.clone(),
    ));
    let admin_router = admin::router(admin_state);

    // 先启动 HTTP 接收器，再回填启动时刻之前的历史记录。
    let (receive_ready_tx, receive_ready_rx) = oneshot::channel();
    let receive_server = qq_receive_server.clone();
    let receive_shutdown = restart.clone();
    let mut qq_receive_task = tokio::spawn(async move {
        receive_server
            .run(receive_ready_tx, admin_router, receive_shutdown)
            .await
    });
    if receive_ready_rx.await.is_err() {
        match (&mut qq_receive_task).await {
            Ok(Ok(())) => anyhow::bail!("HTTP 服务在完成启动通知前退出"),
            Ok(Err(error)) => return Err(error).context("HTTP 服务启动失败"),
            Err(error) => return Err(error).context("HTTP 服务任务异常退出"),
        }
    }

    // 启动时读取一次完整群列表，后续管理页面只使用这份运行时缓存。
    if let Err(error) = qq_receive_server.fetch_group_ids().await {
        warn!(error = %error, "启动时加载群资料失败");
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
        runtime,
    ));
    let mut conversation_dispatcher = ConversationDispatcher::new(
        app_config,
        db_manager,
        message_sender,
        scheduler.clone(),
        message_ingestion,
        platform_rx,
        internal_trigger_rx,
    );

    let mut dispatcher_task = tokio::spawn(async move { conversation_dispatcher.run().await });
    let mut scheduler_task = tokio::spawn(scheduler.run());
    let mut cleanup_task = tokio::spawn(resource_cleanup.run());
    info!(admin_url = "/admin", "服务启动完成");

    let outcome = tokio::select! {
        _ = restart.cancelled() => WorkerExit::Restart,
        result = &mut qq_receive_task => {
            log_task_exit("HTTP 服务", result);
            WorkerExit::Stop
        }
        result = &mut dispatcher_task => {
            log_task_exit("会话分发器", result);
            WorkerExit::Stop
        }
        result = &mut scheduler_task => {
            log_task_exit("定时任务调度器", result);
            WorkerExit::Stop
        }
        result = &mut cleanup_task => {
            log_task_exit("资源清理服务", result);
            WorkerExit::Stop
        }
    };

    restart.cancel();
    // HTTP 先优雅结束，其余循环随后停止；worker 进程退出会回收会话 Actor。
    let _ = tokio::time::timeout(Duration::from_secs(5), &mut qq_receive_task).await;
    dispatcher_task.abort();
    scheduler_task.abort();
    cleanup_task.abort();
    Ok(outcome)
}

fn log_task_exit<T>(name: &str, result: Result<T, tokio::task::JoinError>) {
    match result {
        Ok(_) => warn!(service = name, "后台服务已停止"),
        Err(error) => error!(service = name, error = %error, "后台服务异常退出"),
    }
}

fn init_tracing(configured_level: &str, admin_logs: Arc<admin::AdminLogBuffer>) {
    let default_filter = format!("warn,rolava={}", configured_level);
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .compact()
                .with_filter(env_filter),
        )
        // 管理日志保留全部应用级别，前端可独立切换详细程度。
        .with(admin::AdminLogLayer::new(admin_logs).with_filter(EnvFilter::new("off,rolava=trace")))
        .init();
}
