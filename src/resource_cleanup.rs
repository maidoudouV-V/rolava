use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use std::fs;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, error, info};

use crate::message_enricher::RECEIVED_IMAGE_DIRECTORY;
use crate::repository::db_manager::QQChatContextManager;

const RESOURCE_CLEANUP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// 当前“一个月”按固定 30 天计算。
const RESOURCE_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// 定期清理过期本地资源；后续其它资源类型继续在该服务中追加清理步骤。
pub struct ResourceCleanupService {
    db_manager: Arc<QQChatContextManager>,
    image_root: PathBuf,
}

#[derive(Debug, Default)]
struct CleanupStats {
    scanned: usize,
    deleted: usize,
    failed: usize,
}

impl ResourceCleanupService {
    pub fn new(db_manager: Arc<QQChatContextManager>) -> Self {
        Self::with_image_root(db_manager, RECEIVED_IMAGE_DIRECTORY)
    }

    fn with_image_root(
        db_manager: Arc<QQChatContextManager>,
        image_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            db_manager,
            image_root: image_root.into(),
        }
    }

    /// 启动时立即清理一次，之后每 24 小时重复执行。
    pub async fn run(self: Arc<Self>) {
        info!(retention_days = 30, "资源清理服务已启动");
        let mut cleanup_interval = interval(RESOURCE_CLEANUP_INTERVAL);
        cleanup_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            cleanup_interval.tick().await;
            match self.cleanup_once().await {
                Ok(stats) => info!(
                    scanned = stats.scanned,
                    deleted = stats.deleted,
                    failed = stats.failed,
                    "过期资源清理完成"
                ),
                Err(error) => error!(error = %format!("{error:#}"), "过期资源清理失败"),
            }
        }
    }

    async fn cleanup_once(&self) -> Result<CleanupStats> {
        let cutoff_timestamp = Utc::now().timestamp() - RESOURCE_RETENTION.as_secs() as i64;
        self.cleanup_expired_images(cutoff_timestamp).await
    }

    /// 图片只是当前第一种资源；单个文件失败不会阻止其它候选继续清理。
    async fn cleanup_expired_images(&self, cutoff_timestamp: i64) -> Result<CleanupStats> {
        let images = self
            .db_manager
            .get_received_images_created_before(cutoff_timestamp)?;
        let mut stats = CleanupStats {
            scanned: images.len(),
            ..CleanupStats::default()
        };

        for image in images {
            match self
                .db_manager
                .delete_expired_received_image(&image.image_id, cutoff_timestamp, |path| self.remove_managed_file(path))
            {
                Ok(true) => stats.deleted += 1,
                Ok(false) => {
                    debug!(image_id = %image.image_id, "图片已重新使用或已删除，跳过清理");
                }
                Err(error) => {
                    stats.failed += 1;
                    error!(image_id = %image.image_id, error = %format!("{error:#}"), "清理过期图片失败");
                }
            }
        }

        Ok(stats)
    }

    /// 只允许删除配置资源目录中的普通文件；文件已经不存在时也视为清理成功。
    fn remove_managed_file(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let canonical_path = match fs::canonicalize(path) {
            Ok(path) => path,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("读取资源路径失败: {}", path.display()))
            }
        };
        let canonical_root = fs::canonicalize(&self.image_root)
            .with_context(|| format!("读取图片资源目录失败: {}", self.image_root.display()))?;
        if !canonical_path.starts_with(&canonical_root) {
            anyhow::bail!(
                "拒绝删除图片资源目录之外的文件: {}",
                canonical_path.display()
            );
        }

        fs::remove_file(&canonical_path)
            .with_context(|| format!("删除资源文件失败: {}", canonical_path.display()))?;
        Ok(())
    }
}
