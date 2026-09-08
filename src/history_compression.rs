use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Local, LocalResult, NaiveDate, TimeZone, Utc, Weekday};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::ai_provider::{
    run_ai_request_with_timeout, ToolChatMessage, ToolChatResponse, ToolChatUserContent,
};
use crate::chat_history::render_history_message_line;
use crate::config::AppConfig;
use crate::conversation_trigger::{ConversationTrigger, RoutedConversationTrigger};
use crate::repository::db_manager::{
    ChatMessage, NewConversationDailySummary, PendingDailySummary, QQChatContextManager,
};
use crate::runtime_state::RuntimeState;
use crate::tools::ToolRegistry;
use crate::transport::message::{Conversation, ConversationKind, MessageTarget};

const COMPRESSION_RUN_HOUR: u32 = 3;

/// 按会话和统计日调用主模型，将已经结束的聊天记录压缩为长期摘要。
pub struct HistoryCompressionService {
    app_config: Arc<AppConfig>,
    db_manager: Arc<QQChatContextManager>,
    runtime_state: Arc<RuntimeState>,
    trigger_tx: tokio::sync::mpsc::UnboundedSender<RoutedConversationTrigger>,
}

#[derive(Default)]
struct CompressionStats {
    pending: usize,
    inserted: usize,
    failed: usize,
}

impl HistoryCompressionService {
    pub fn new(
        app_config: Arc<AppConfig>,
        db_manager: Arc<QQChatContextManager>,
        runtime_state: Arc<RuntimeState>,
        trigger_tx: tokio::sync::mpsc::UnboundedSender<RoutedConversationTrigger>,
    ) -> Self {
        Self {
            app_config,
            db_manager,
            runtime_state,
            trigger_tx,
        }
    }

    /// 启动时若已启用，立即补偿今天之前的记录，之后在本地时间 03:00 执行。
    pub async fn run(self: Arc<Self>) {
        if !self.app_config.app.history_summary_enabled {
            // 保持受主进程监管的任务存活，关闭摘要不应触发整个服务退出。
            std::future::pending::<()>().await;
        }
        info!(run_hour = COMPRESSION_RUN_HOUR, "聊天记录压缩服务已启动");
        self.run_and_log(true).await;
        loop {
            let now = Local::now();
            let next_run = match next_compression_run(now) {
                Ok(value) => value,
                Err(error) => {
                    error!(error = %format!("{error:#}"), "计算下次聊天记录压缩时间失败");
                    sleep(Duration::from_secs(60)).await;
                    continue;
                }
            };
            let wait = (next_run - Local::now()).to_std().unwrap_or(Duration::ZERO);
            info!(next_run = %next_run.format("%Y-%m-%d %H:%M:%S"), "等待下次聊天记录压缩");
            sleep(wait).await;
            self.run_and_log(false).await;
        }
    }

    async fn run_and_log(&self, immediate: bool) {
        match self.run_once(Local::now(), immediate).await {
            Ok(stats) => info!(
                pending = stats.pending,
                inserted = stats.inserted,
                failed = stats.failed,
                "聊天记录压缩完成"
            ),
            Err(error) => error!(error = %format!("{error:#}"), "扫描待压缩聊天记录失败"),
        }
    }

    async fn run_once(&self, now: DateTime<Local>, immediate: bool) -> Result<CompressionStats> {
        let closed_before = if immediate {
            local_time(now.date_naive(), 0)?
        } else {
            last_closed_boundary(now)?
        }
        .timestamp();
        let tasks = self.db_manager.get_pending_daily_summaries(closed_before)?;
        let mut stats = CompressionStats {
            pending: tasks.len(),
            ..CompressionStats::default()
        };

        // 每个统计日独立失败，不能让一条超长记录阻塞其它会话的补偿任务。
        for task in tasks {
            debug!(
                source = %task.source,
                conversation_id = %task.source_conversation_id,
                summary_date = %task.summary_date,
                "开始压缩会话当日聊天记录"
            );
            match self.compress_task(&task).await {
                Ok(true) => stats.inserted += 1,
                Ok(false) => {}
                Err(error) => {
                    stats.failed += 1;
                    warn!(
                        source = %task.source,
                        conversation_id = %task.source_conversation_id,
                        summary_date = %task.summary_date,
                        error = %format!("{error:#}"),
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
        let (period_start, period_end) = summary_period(&task.summary_date)?;
        let previous_date = period_start
            .date_naive()
            .pred_opt()
            .context("无法计算前一天摘要日期")?
            .format("%Y-%m-%d")
            .to_string();
        let previous_summaries = self.db_manager.get_conversation_daily_summaries_between(
            task.conversation_id,
            &previous_date,
            &previous_date,
        )?;
        let following_messages = self.db_manager.get_summary_messages_in_period(
            task.conversation_id,
            period_end.timestamp(),
            local_time(period_end.date_naive(), COMPRESSION_RUN_HOUR)?.timestamp(),
        )?;
        let bot_id = self.runtime_state.bot_id();
        let bot_name = self.runtime_state.bot_name();
        let request_messages = build_summary_request(
            &self.app_config.prompt_config.chat_history_summary_prompt,
            &task.summary_date,
            &messages,
            previous_summaries
                .first()
                .map(|summary| summary.summary_text.as_str()),
            &following_messages,
            bot_id.as_deref(),
            bot_name.as_deref(),
        )?;
        let summary_text = self.request_summary(&request_messages).await?;
        // 消息已经按事件时间和 ID 排序，首尾 ID 必须反映实际总结输入的顺序。
        let source_first_message_id = messages.first().expect("消息列表已检查为非空").id;
        let source_last_message_id = messages.last().expect("消息列表已检查为非空").id;
        let inserted =
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
                })?;
        if inserted && ToolRegistry::is_enabled(&self.app_config.app.enabled_actions, "memory") {
            if let Err(error) = self.trigger_memory_review(task, &summary_text) {
                warn!(error = %format!("{error:#}"), summary_date = %task.summary_date, "摘要已保存，但记忆整理任务投递失败");
            }
        }
        Ok(inserted)
    }

    fn trigger_memory_review(&self, task: &PendingDailySummary, summary: &str) -> Result<()> {
        let conversation = self
            .db_manager
            .get_conversation_by_id(task.conversation_id)?
            .context("摘要会话不存在")?;
        let kind = match conversation.kind.as_str() {
            "group" => ConversationKind::Group,
            "direct" => ConversationKind::Direct,
            kind => anyhow::bail!("未知会话类型：{}", kind),
        };
        let user_prompt = crate::config::render_prompt_template(
            &self.app_config.prompt_config.memory_review_prompt,
            &[
                ("summary_date", &task.summary_date),
                ("daily_summary", summary),
            ],
        );
        self.trigger_tx
            .send(RoutedConversationTrigger {
                target: MessageTarget {
                    source: task.source.clone(),
                    bot_id: self
                        .runtime_state
                        .bot_id()
                        .context("记忆整理缺少机器人账号")?,
                    conversation: Conversation {
                        id: task.source_conversation_id.clone(),
                        kind,
                        title: conversation.title,
                    },
                },
                trigger: ConversationTrigger {
                    user_prompt,
                    memory_review: true,
                    condition: None,
                },
            })
            .context("投递记忆整理任务失败")?;
        Ok(())
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
            let result =
                run_ai_request_with_timeout(timeout_seconds, "聊天记录压缩请求", request).await;
            match result.and_then(completed_summary_text) {
                Ok(content) => return Ok(content),
                Err(error) => {
                    warn!(attempt, max_attempts, error = %format!("{error:#}"), "聊天记录压缩请求失败，准备重试");
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
    summary_date: &str,
    messages: &[ChatMessage],
    previous_summary: Option<&str>,
    following_messages: &[ChatMessage],
    bot_id: Option<&str>,
    bot_name: Option<&str>,
) -> Result<Vec<ToolChatMessage>> {
    let history = render_daily_messages(messages, bot_id, bot_name)?;
    let mut content = format!("本次总结日期：{summary_date}（本地时间 00:00:00 至次日 00:00:00，不含终点）。只总结此日期内的内容。\n");
    if let Some(summary) = previous_summary {
        content.push_str(&format!(
            "\n# 前一天摘要（仅供连贯性参考，不纳入本次摘要）\n{summary}\n"
        ));
    }
    content.push_str(&format!("\n# 当天聊天记录（本次总结对象）\n{history}\n"));
    if !following_messages.is_empty() {
        content.push_str(&format!(
            "\n# 次日 00:00～03:00 聊天记录（不含 03:00，仅供连贯性参考，不纳入本次摘要）\n{}\n",
            render_daily_messages(following_messages, bot_id, bot_name)?
        ));
    }
    Ok(vec![
        ToolChatMessage::System {
            content: system_prompt.to_string(),
        },
        ToolChatMessage::User {
            content: ToolChatUserContent::text(content),
        },
    ])
}

fn render_daily_messages(
    messages: &[ChatMessage],
    bot_id: Option<&str>,
    bot_name: Option<&str>,
) -> Result<String> {
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
        if bot_id == Some(message.sender_id.as_str()) {
            if let Some(bot_name) = bot_name.filter(|name| !name.trim().is_empty()) {
                let time = local.format("%H:%M:%S");
                let content = message.content_text.as_deref().unwrap_or_default();
                rendered.push_str(&format!("{}（{}）:{}", bot_name.trim(), time, content));
                continue;
            }
        }
        rendered.push_str(&render_history_message_line(message, &local));
    }
    Ok(rendered)
}

/// 将平台时间戳映射到以本地时间 00:00 为起点的自然日。
pub fn summary_date_for_timestamp(timestamp: i64) -> Result<String> {
    let utc = DateTime::<Utc>::from_timestamp(timestamp, 0).context("消息时间戳无效")?;
    let local = DateTime::<Local>::from(utc);
    let date = local.date_naive();
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

fn local_time(date: NaiveDate, hour: u32) -> Result<DateTime<Local>> {
    let naive = date
        .and_hms_opt(hour, 0, 0)
        .context("无法构造聊天记录压缩时间")?;
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(value) => Ok(value),
        LocalResult::Ambiguous(first, _) => Ok(first),
        LocalResult::None => anyhow::bail!("本地时区不存在 {} {:02}:00", date, hour),
    }
}

pub(crate) fn summary_period(summary_date: &str) -> Result<(DateTime<Local>, DateTime<Local>)> {
    let date = parse_summary_date(summary_date)?;
    let next_date = date.succ_opt().context("无法计算摘要统计日终点")?;
    Ok((local_time(date, 0)?, local_time(next_date, 0)?))
}

fn last_closed_boundary(now: DateTime<Local>) -> Result<DateTime<Local>> {
    let today = now.date_naive();
    let today_run = local_time(today, COMPRESSION_RUN_HOUR)?;
    if now >= today_run {
        local_time(today, 0)
    } else {
        local_time(today.pred_opt().context("无法计算前一统计日")?, 0)
    }
}

fn next_compression_run(now: DateTime<Local>) -> Result<DateTime<Local>> {
    let today = now.date_naive();
    let today_boundary = local_time(today, COMPRESSION_RUN_HOUR)?;
    if now < today_boundary {
        Ok(today_boundary)
    } else {
        local_time(
            today.succ_opt().context("无法计算下一统计日")?,
            COMPRESSION_RUN_HOUR,
        )
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone};

    use super::{
        last_closed_boundary, next_compression_run, summary_date_for_timestamp, summary_period,
    };

    // 验证自然日零点边界与凌晨三点执行时间互不混淆，重启也不会提前总结。
    #[test]
    fn summary_day_changes_at_midnight_but_runs_at_three_am() {
        let before = Local
            .with_ymd_and_hms(2026, 8, 24, 23, 59, 59)
            .single()
            .unwrap();
        let boundary = Local
            .with_ymd_and_hms(2026, 8, 25, 0, 0, 0)
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
        let run = Local
            .with_ymd_and_hms(2026, 8, 25, 3, 0, 0)
            .single()
            .unwrap();
        let (start, end) = summary_period("2026-08-24").unwrap();
        assert_eq!(end, boundary);
        assert_eq!(
            last_closed_boundary(run - chrono::Duration::seconds(1)).unwrap(),
            start
        );
        assert_eq!(last_closed_boundary(run).unwrap(), end);
        assert_eq!(next_compression_run(boundary).unwrap(), run);
        assert_eq!(
            next_compression_run(run).unwrap(),
            Local
                .with_ymd_and_hms(2026, 8, 26, 3, 0, 0)
                .single()
                .unwrap()
        );
    }
}
