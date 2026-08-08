use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use tokio::fs;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, error, info, warn};

use crate::repository::db_manager::QQChatContextManager;

const RESOURCE_CLEANUP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// 当前“一个月”按固定 30 天计算。
const RESOURCE_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// 定期清理过期本地资源；后续其它资源类型继续在该服务中追加清理步骤。
pub struct ResourceCleanupService {
    db_manager: Arc<QQChatContextManager>,
    received_image_dir: PathBuf,
}

#[derive(Debug, Default)]
struct CleanupStats {
    scanned: usize,
    deleted: usize,
    failed: usize,
}

impl ResourceCleanupService {
    pub fn new(
        db_manager: Arc<QQChatContextManager>,
        received_image_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            db_manager,
            received_image_dir: received_image_dir.into(),
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
                Err(error) => error!(error = %error, "过期资源清理失败"),
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
            if let Err(error) = self.remove_managed_file(&image.local_path).await {
                stats.failed += 1;
                warn!(
                    image_id = %image.image_id,
                    path = %image.local_path,
                    error = %error,
                    "删除过期图片文件失败，保留数据库记录"
                );
                continue;
            }

            match self
                .db_manager
                .delete_received_image_created_before(&image.image_id, cutoff_timestamp)
            {
                Ok(true) => stats.deleted += 1,
                Ok(false) => {
                    debug!(image_id = %image.image_id, "过期图片记录已被其它流程删除");
                    stats.deleted += 1;
                }
                Err(error) => {
                    stats.failed += 1;
                    error!(image_id = %image.image_id, error = %error, "删除过期图片数据库记录失败");
                }
            }
        }

        Ok(stats)
    }

    /// 只允许删除配置资源目录中的普通文件；文件已经不存在时也视为清理成功。
    async fn remove_managed_file(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let canonical_path = match fs::canonicalize(path).await {
            Ok(path) => path,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("读取资源路径失败: {}", path.display()))
            }
        };
        let canonical_root = fs::canonicalize(&self.received_image_dir)
            .await
            .with_context(|| {
                format!(
                    "读取图片资源目录失败: {}",
                    self.received_image_dir.display()
                )
            })?;
        if !canonical_path.starts_with(&canonical_root) {
            anyhow::bail!(
                "拒绝删除图片资源目录之外的文件: {}",
                canonical_path.display()
            );
        }

        fs::remove_file(&canonical_path)
            .await
            .with_context(|| format!("删除资源文件失败: {}", canonical_path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use tokio::fs;

    use super::ResourceCleanupService;
    use crate::repository::db_manager::{NewReceivedImage, QQChatContextManager};

    #[tokio::test]
    async fn expired_image_file_and_database_record_are_deleted() {
        let root = std::env::temp_dir().join(format!(
            "rolava-resource-cleanup-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let image_dir = root.join("images");
        fs::create_dir_all(&image_dir).await.unwrap();
        let image_path = image_dir.join("img_test.png");
        fs::write(&image_path, b"image").await.unwrap();

        let db_path = root.join("test.db");
        let db_manager = Arc::new(QQChatContextManager::new(db_path.to_str().unwrap()).unwrap());
        db_manager
            .insert_received_image(&NewReceivedImage {
                image_id: "img_test".to_string(),
                content_hash: "hash_test".to_string(),
                local_path: image_path.to_string_lossy().to_string(),
                original_url: None,
                mime_type: Some("image/png".to_string()),
                file_size: 5,
                description: String::new(),
                metadata_json: "{}".to_string(),
            })
            .unwrap();

        let service = ResourceCleanupService::new(db_manager.clone(), &image_dir);
        let stats = service
            .cleanup_expired_images(Utc::now().timestamp() + 1)
            .await
            .unwrap();

        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.deleted, 1);
        assert_eq!(stats.failed, 0);
        assert!(!image_path.exists());
        assert!(db_manager
            .get_received_image_by_id("img_test")
            .unwrap()
            .is_none());

        drop(service);
        drop(db_manager);
        fs::remove_dir_all(root).await.unwrap();
    }
}
