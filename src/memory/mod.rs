mod group_memory;
mod member_profile;
mod user_memory;

pub use group_memory::{GroupMemorySession, MAX_RETENTION_DAYS, SECONDS_PER_DAY};
pub use user_memory::{UserMemoryService, UserMemorySession};

pub fn format_memory_time(timestamp: i64) -> anyhow::Result<String> {
    let time = chrono::DateTime::from_timestamp(timestamp, 0)
        .ok_or_else(|| anyhow::anyhow!("记忆更新时间无效"))?;
    Ok(time
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d")
        .to_string())
}
