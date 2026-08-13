use std::str::FromStr;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, LocalResult, NaiveDateTime, TimeZone};
use croner::Cron;
use serde::Serialize;

pub const SCHEDULE_TITLE_MAX_CHARS: usize = 50;
pub const SCHEDULE_INSTRUCTION_MAX_CHARS: usize = 1000;
const SCHEDULE_MAX_CHARS: usize = 100;
const AT_PREFIX: &str = "at:";
const CRON_PREFIX: &str = "cron:";
const LOCAL_DATETIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// 数据库中的一条定时任务，以及投递任务所需的会话地址。
#[derive(Debug, Clone)]
pub struct ScheduledTask {
    pub id: String,
    pub conversation_id: i64,
    pub source: String,
    pub source_conversation_id: String,
    pub conversation_kind: String,
    pub conversation_title: Option<String>,
    pub bot_id: String,
    pub title: String,
    pub schedule: String,
    pub instruction: String,
    pub next_run_at: i64,
    pub last_triggered_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 注入系统提示词的精简任务信息，详细说明通过查询工具按需读取。
#[derive(Debug, Serialize)]
pub struct ScheduledTaskSummary<'a> {
    pub task_id: &'a str,
    pub title: &'a str,
    pub schedule: &'a str,
}

impl ScheduledTask {
    pub fn summary(&self) -> ScheduledTaskSummary<'_> {
        ScheduledTaskSummary {
            task_id: &self.id,
            title: &self.title,
            schedule: &self.schedule,
        }
    }
}

/// 校验外部时间表达式，并计算严格晚于 `after` 的下一次本地执行时间。
pub fn calculate_next_run(schedule: &str, after: DateTime<Local>) -> Result<DateTime<Local>> {
    let schedule = schedule.trim();
    validate_text_length("时间表达式", schedule, 1, SCHEDULE_MAX_CHARS)?;

    if let Some(value) = schedule.strip_prefix(AT_PREFIX) {
        return parse_one_time_schedule(value.trim(), after);
    }
    if let Some(value) = schedule.strip_prefix(CRON_PREFIX) {
        return parse_cron_schedule(value.trim(), after);
    }

    bail!("时间表达式必须使用 at:YYYY-MM-DD HH:MM:SS 或 cron:分 时 日 月 周")
}

pub fn validate_task_text(title: &str, instruction: &str) -> Result<()> {
    validate_text_length("任务标题", title.trim(), 1, SCHEDULE_TITLE_MAX_CHARS)?;
    if title.contains(['\r', '\n']) {
        bail!("任务标题必须是单行文本");
    }
    validate_text_length(
        "任务说明",
        instruction.trim(),
        1,
        SCHEDULE_INSTRUCTION_MAX_CHARS,
    )
}

fn parse_one_time_schedule(value: &str, after: DateTime<Local>) -> Result<DateTime<Local>> {
    let naive = NaiveDateTime::parse_from_str(value, LOCAL_DATETIME_FORMAT)
        .context("单次任务时间格式无效，应为 YYYY-MM-DD HH:MM:SS")?;
    let local_time = match Local.from_local_datetime(&naive) {
        LocalResult::Single(time) => time,
        LocalResult::Ambiguous(_, _) => bail!("单次任务时间在当前系统时区中存在歧义"),
        LocalResult::None => bail!("单次任务时间在当前系统时区中不存在"),
    };
    if local_time <= after {
        bail!("单次任务时间必须晚于当前时间");
    }
    Ok(local_time)
}

fn parse_cron_schedule(value: &str, after: DateTime<Local>) -> Result<DateTime<Local>> {
    if value.split_whitespace().count() != 5 {
        bail!("周期任务必须使用五段 cron 表达式：分 时 日 月 周");
    }
    let cron = Cron::from_str(value).context("cron 表达式无效")?;
    cron.find_next_occurrence(&after, false)
        .context("无法计算 cron 表达式的下一次执行时间")
}

fn validate_text_length(name: &str, value: &str, min: usize, max: usize) -> Result<()> {
    let length = value.chars().count();
    if !(min..=max).contains(&length) {
        bail!("{}长度必须在 {} 到 {} 个字符之间", name, min, max);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Local};

    use super::calculate_next_run;

    // 验证五段 cron 表达式能计算下次执行时间。
    #[test]
    fn parses_five_field_cron() {
        let now = Local::now();
        let next = calculate_next_run("cron:0 9 * * *", now).unwrap();
        assert!(next > now);
    }

    // 验证过去的一次性执行时间会被拒绝。
    #[test]
    fn rejects_past_one_time_schedule() {
        let now = Local::now();
        let past = (now - Duration::days(1))
            .format("at:%Y-%m-%d %H:%M:%S")
            .to_string();
        assert!(calculate_next_run(&past, now).is_err());
    }
}
