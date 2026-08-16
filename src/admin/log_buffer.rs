use std::collections::VecDeque;
use std::fmt::Debug;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::Mutex;
use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

const MAX_LOG_TEXT_BYTES: usize = 32 * 1024;

/// 管理页面可读取的一条结构化运行日志。
#[derive(Debug, Clone, Serialize)]
pub struct AdminLogEntry {
    pub id: u64,
    pub timestamp: i64,
    pub level: String,
    pub target: String,
    pub message: String,
    pub fields: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdminLogPage {
    pub items: Vec<AdminLogEntry>,
    pub latest_id: u64,
    /// 客户端游标早于当前最老日志时为 true，表示中间日志已经被覆盖。
    pub truncated: bool,
}

struct AdminLogState {
    next_id: u64,
    entries: VecDeque<AdminLogEntry>,
}

/// 固定容量的内存日志；写满后移除最老记录，内存不会随运行时间无限增长。
pub struct AdminLogBuffer {
    capacity: usize,
    state: Mutex<AdminLogState>,
}

impl AdminLogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            state: Mutex::new(AdminLogState {
                next_id: 1,
                entries: VecDeque::with_capacity(capacity.max(1)),
            }),
        }
    }

    fn push(&self, mut entry: AdminLogEntry) {
        // 正文和字段各限制 32 KiB，使单条日志总文本不会超过约 64 KiB。
        truncate_utf8(&mut entry.message, MAX_LOG_TEXT_BYTES);
        truncate_utf8(&mut entry.fields, MAX_LOG_TEXT_BYTES);

        let mut state = self.state.lock();
        entry.id = state.next_id;
        state.next_id = state.next_id.saturating_add(1);
        if state.entries.len() == self.capacity {
            state.entries.pop_front();
        }
        state.entries.push_back(entry);
    }

    /// 无游标时返回最新一页；带游标时返回之后产生的日志。
    pub fn read_after(&self, after_id: Option<u64>, limit: usize) -> AdminLogPage {
        let state = self.state.lock();
        let limit = limit.clamp(1, 500);
        let oldest_id = state.entries.front().map(|entry| entry.id);
        let truncated = after_id.is_some_and(|after_id| {
            oldest_id.is_some_and(|oldest_id| after_id.saturating_add(1) < oldest_id)
        });
        let items = match after_id {
            Some(after_id) => state
                .entries
                .iter()
                .filter(|entry| entry.id > after_id)
                .take(limit)
                .cloned()
                .collect(),
            None => state
                .entries
                .iter()
                .skip(state.entries.len().saturating_sub(limit))
                .cloned()
                .collect(),
        };
        AdminLogPage {
            items,
            latest_id: state.entries.back().map(|entry| entry.id).unwrap_or(0),
            truncated,
        }
    }
}

/// 将 tracing 事件转换为管理页面使用的结构化日志。
pub struct AdminLogLayer {
    buffer: Arc<AdminLogBuffer>,
}

impl AdminLogLayer {
    pub fn new(buffer: Arc<AdminLogBuffer>) -> Self {
        Self { buffer }
    }
}

impl<S> Layer<S> for AdminLogLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = AdminLogVisitor::default();
        event.record(&mut visitor);
        self.buffer.push(AdminLogEntry {
            id: 0,
            timestamp: Utc::now().timestamp(),
            level: metadata.level().as_str().to_string(),
            target: metadata.target().to_string(),
            message: visitor.message.unwrap_or_default(),
            fields: visitor
                .fields
                .into_iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join(" "),
        });
    }
}

#[derive(Default)]
struct AdminLogVisitor {
    message: Option<String>,
    fields: Vec<(String, String)>,
}

impl AdminLogVisitor {
    fn record_value(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = Some(value);
        } else {
            self.fields.push((field.name().to_string(), value));
        }
    }
}

impl Visit for AdminLogVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_value(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.record_value(field, format!("{value:?}"));
    }
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value.push('…');
}

#[cfg(test)]
mod tests {
    use super::{AdminLogBuffer, AdminLogEntry};

    #[test]
    fn ring_buffer_discards_oldest_entries_and_reports_cursor_gap() {
        let buffer = AdminLogBuffer::new(2);
        for message in ["one", "two", "three"] {
            buffer.push(AdminLogEntry {
                id: 0,
                timestamp: 1,
                level: "INFO".to_string(),
                target: "rolava".to_string(),
                message: message.to_string(),
                fields: String::new(),
            });
        }

        let page = buffer.read_after(Some(0), 10);
        assert!(page.truncated);
        assert_eq!(page.latest_id, 3);
        assert_eq!(
            page.items
                .iter()
                .map(|entry| entry.message.as_str())
                .collect::<Vec<_>>(),
            vec!["two", "three"]
        );
    }
}
