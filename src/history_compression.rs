use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Local, LocalResult, NaiveDate, TimeZone, Timelike, Utc, Weekday};
use tokio::time::{sleep, timeout};
use tracing::{error, info, warn};

use crate::ai_provider::{ToolChatMessage, ToolChatResponse, ToolChatUserContent};
use crate::chat_history::render_history_message_line;
use crate::config::AppConfig;
use crate::repository::db_manager::{
    ChatMessage, NewConversationDailySummary, PendingDailySummary, QQChatContextManager,
};

const DAILY_BOUNDARY_HOUR: u32 = 3;

/// 按会话和统计日调用主模型，将已经结束的聊天记录压缩为长期摘要。
pub struct HistoryCompressionService {
    app_config: Arc<AppConfig>,
    db_manager: Arc<QQChatContextManager>,
}

#[derive(Default)]
struct CompressionStats {
    pending: usize,
    inserted: usize,
    failed: usize,
}

impl HistoryCompressionService {
    pub fn new(app_config: Arc<AppConfig>, db_manager: Arc<QQChatContextManager>) -> Self {
        Self {
            app_config,
            db_manager,
        }
    }

    /// 启动时立即补偿一次，之后始终等待到下一个本地时间 03:00。
    pub async fn run(self: Arc<Self>) {
        info!(
            boundary_hour = DAILY_BOUNDARY_HOUR,
            "聊天记录压缩服务已启动"
        );
        self.run_and_log().await;
        loop {
            let now = Local::now();
            let next_run = match next_compression_run(now) {
                Ok(value) => value,
                Err(error) => {
                    error!(error = %error, "计算下次聊天记录压缩时间失败");
                    sleep(Duration::from_secs(60)).await;
                    continue;
                }
            };
            let wait = (next_run - Local::now()).to_std().unwrap_or(Duration::ZERO);
            info!(next_run = %next_run.format("%Y-%m-%d %H:%M:%S"), "等待下次聊天记录压缩");
            sleep(wait).await;
            self.run_and_log().await;
        }
    }

    async fn run_and_log(&self) {
        match self.run_once(Local::now()).await {
            Ok(stats) => info!(
                pending = stats.pending,
                inserted = stats.inserted,
                failed = stats.failed,
                "聊天记录压缩完成"
            ),
            Err(error) => error!(error = %error, "扫描待压缩聊天记录失败"),
        }
    }

    async fn run_once(&self, now: DateTime<Local>) -> Result<CompressionStats> {
        let closed_before = last_closed_boundary(now)?.timestamp();
        let tasks = self.db_manager.get_pending_daily_summaries(closed_before)?;
        let mut stats = CompressionStats {
            pending: tasks.len(),
            ..CompressionStats::default()
        };

        // 每个统计日独立失败，不能让一条超长记录阻塞其它会话的补偿任务。
        for task in tasks {
            match self.compress_task(&task).await {
                Ok(true) => stats.inserted += 1,
                Ok(false) => {}
                Err(error) => {
                    stats.failed += 1;
                    warn!(
                        source = %task.source,
                        conversation_id = %task.source_conversation_id,
                        summary_date = %task.summary_date,
                        error = %error,
                        "聊天记录压缩失败，保留到下次重试"
                    );
                }
            }
        }
        Ok(stats)
    }

    async fn compress_task(&self, task: &PendingDailySummary) -> Result<bool> {
        let messages = self
            .db_manager
            .get_daily_summary_source_messages(task.conversation_id, &task.summary_date)?;
        if messages.is_empty() {
            return Ok(false);
        }
        let request_messages = build_summary_request(
            &self.app_config.prompt_config.chat_history_summary_prompt,
            &messages,
        )?;
        let summary_text = self.request_summary(&request_messages).await?;
        let (period_start, period_end) = summary_period(&task.summary_date)?;
        // 消息已经按事件时间和 ID 排序，首尾 ID 必须反映实际总结输入的顺序。
        let source_first_message_id = messages.first().expect("消息列表已检查为非空").id;
        let source_last_message_id = messages.last().expect("消息列表已检查为非空").id;
        self.db_manager
            .insert_conversation_daily_summary(&NewConversationDailySummary {
                conversation_id: task.conversation_id,
                summary_date: &task.summary_date,
                period_start: period_start.timestamp(),
                period_end: period_end.timestamp(),
                summary_text: &summary_text,
                source_message_count: messages.len() as i64,
                source_first_message_id,
                source_last_message_id,
            })
    }

    async fn request_summary(&self, messages: &[ToolChatMessage]) -> Result<String> {
        let max_attempts = self.app_config.app.ai_request_max_attempts();
        let timeout_seconds = self.app_config.app.ai_request_timeout_seconds;
        let mut last_error = None;
        for attempt in 1..=max_attempts {
            let provider = self
                .app_config
                .ai_models
                .get(&self.app_config.app.chat_model_name)
                .expect("找不到聊天模型配置");
            let request = provider.chat_completions(messages, &[]);
            let result = if timeout_seconds == 0 {
                request.await
            } else {
                match timeout(Duration::from_secs(timeout_seconds), request).await {
                    Ok(result) => result,
                    Err(_) => Err(anyhow::anyhow!(
                        "聊天记录压缩请求超时，超过 {} 秒",
                        timeout_seconds
                    )),
                }
            };
            match result.and_then(completed_summary_text) {
                Ok(content) => return Ok(content),
                Err(error) => {
                    warn!(attempt, max_attempts, error = %error, "聊天记录压缩请求失败，准备重试");
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.expect("聊天记录压缩重试循环至少应执行一次"))
    }
}

/// 仅保存明确正常结束的摘要，截断或未知状态交给现有失败重试流程。
fn completed_summary_text(response: ToolChatResponse) -> Result<String> {
    if !matches!(response.finish_reason.as_deref(), Some("stop" | "STOP")) {
        anyhow::bail!(
            "聊天记录压缩未正常完成，结束原因：{:?}",
            response.finish_reason
        );
    }
    if !response.tool_calls.is_empty() {
        anyhow::bail!("聊天记录压缩响应包含未执行的工具调用");
    }
    response
        .content
        .map(|content| content.trim().to_string())
        .filter(|content| !content.is_empty())
        .context("聊天记录压缩响应正文为空")
}

/// 生成固定的 system + 单个 user 请求，不混入普通聊天提示词或工具定义。
fn build_summary_request(
    system_prompt: &str,
    messages: &[ChatMessage],
) -> Result<Vec<ToolChatMessage>> {
    let history = render_daily_messages(messages)?;
    Ok(vec![
        ToolChatMessage::System {
            content: system_prompt.to_string(),
        },
        ToolChatMessage::User {
            content: ToolChatUserContent::text(format!("需要压缩的聊天记录：\n{}", history)),
        },
    ])
}

fn render_daily_messages(messages: &[ChatMessage]) -> Result<String> {
    let mut rendered = String::new();
    let mut last_date = None;
    for message in messages {
        let utc = DateTime::<Utc>::from_timestamp(message.event_timestamp, 0)
            .context("聊天记录包含无效时间戳")?;
        let local = DateTime::<Local>::from(utc);
        let date = local.format("%Y-%m-%d").to_string();
        if last_date.as_deref() != Some(date.as_str()) {
            if !rendered.is_empty() {
                rendered.push('\n');
            }
            rendered.push_str(&date);
            last_date = Some(date);
        }
        rendered.push('\n');
        rendered.push_str(&render_history_message_line(message, &local));
    }
    Ok(rendered)
}

/// 将平台时间戳映射到以本地时间 03:00 为起点的统计日期。
pub fn summary_date_for_timestamp(timestamp: i64) -> Result<String> {
    let utc = DateTime::<Utc>::from_timestamp(timestamp, 0).context("消息时间戳无效")?;
    let local = DateTime::<Local>::from(utc);
    let date = if local.hour() < DAILY_BOUNDARY_HOUR {
        local
            .date_naive()
            .pred_opt()
            .context("无法计算消息所属的前一统计日")?
    } else {
        local.date_naive()
    };
    Ok(date.format("%Y-%m-%d").to_string())
}

/// 将摘要统计日期渲染成稳定的中文日期标题。
pub fn render_summary_date_heading(summary_date: &str) -> Result<String> {
    let date = parse_summary_date(summary_date)?;
    let weekday = match date.weekday() {
        Weekday::Mon => "一",
        Weekday::Tue => "二",
        Weekday::Wed => "三",
        Weekday::Thu => "四",
        Weekday::Fri => "五",
        Weekday::Sat => "六",
        Weekday::Sun => "日",
    };
    Ok(format!(
        "{}年{}月{}日 星期{}",
        date.year(),
        date.month(),
        date.day(),
        weekday
    ))
}

fn parse_summary_date(summary_date: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(summary_date, "%Y-%m-%d")
        .with_context(|| format!("摘要统计日期格式无效：{}", summary_date))
}

fn local_boundary(date: NaiveDate) -> Result<DateTime<Local>> {
    let naive = date
        .and_hms_opt(DAILY_BOUNDARY_HOUR, 0, 0)
        .context("无法构造聊天记录压缩时间")?;
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(value) => Ok(value),
        LocalResult::Ambiguous(first, _) => Ok(first),
        LocalResult::None => anyhow::bail!("本地时区不存在 {} 03:00", date),
    }
}

fn summary_period(summary_date: &str) -> Result<(DateTime<Local>, DateTime<Local>)> {
    let date = parse_summary_date(summary_date)?;
    let next_date = date.succ_opt().context("无法计算摘要统计日终点")?;
    Ok((local_boundary(date)?, local_boundary(next_date)?))
}

fn last_closed_boundary(now: DateTime<Local>) -> Result<DateTime<Local>> {
    let today = now.date_naive();
    let today_boundary = local_boundary(today)?;
    if now >= today_boundary {
        Ok(today_boundary)
    } else {
        local_boundary(today.pred_opt().context("无法计算前一统计日")?)
    }
}

fn next_compression_run(now: DateTime<Local>) -> Result<DateTime<Local>> {
    let today = now.date_naive();
    let today_boundary = local_boundary(today)?;
    if now < today_boundary {
        Ok(today_boundary)
    } else {
        local_boundary(today.succ_opt().context("无法计算下一统计日")?)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone};

    use super::summary_date_for_timestamp;

    // 验证凌晨三点是统计日唯一边界，边界前一分钟仍属于前一天。
    #[test]
    fn summary_day_changes_at_three_am() {
        let before = Local
            .with_ymd_and_hms(2026, 8, 25, 2, 59, 0)
            .single()
            .unwrap();
        let boundary = Local
            .with_ymd_and_hms(2026, 8, 25, 3, 0, 0)
            .single()
            .unwrap();

        assert_eq!(
            summary_date_for_timestamp(before.timestamp()).unwrap(),
            "2026-08-24"
        );
        assert_eq!(
            summary_date_for_timestamp(boundary.timestamp()).unwrap(),
            "2026-08-25"
        );
    }
}
