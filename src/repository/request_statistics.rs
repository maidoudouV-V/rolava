use std::path::Path;

use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

use crate::ai_provider::ChatUsage;
use crate::request_statistics::{RequestRecord, RequestStatisticsPage, RequestSummary};

/// 独立的 SQLite 统计库，时间索引用于范围汇总与游标分页。
pub struct RequestStatisticsStore {
    pool: Pool<SqliteConnectionManager>,
}

impl RequestStatisticsStore {
    pub fn new(path: &str) -> Result<Self> {
        if let Some(parent) = Path::new(path)
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let manager = SqliteConnectionManager::file(path).with_init(|connection| {
            connection.execute_batch(
                "PRAGMA busy_timeout = 5000;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;",
            )
        });
        let pool = Pool::builder().max_size(3).build(manager)?;
        pool.get()?.execute_batch(
            "CREATE TABLE IF NOT EXISTS ai_requests (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                started_at INTEGER NOT NULL,
                provider_type TEXT NOT NULL,
                provider_name TEXT NOT NULL,
                model_name TEXT NOT NULL,
                model TEXT NOT NULL,
                operation TEXT NOT NULL,
                outcome TEXT NOT NULL,
                http_status INTEGER,
                elapsed_ms INTEGER NOT NULL,
                prompt_tokens INTEGER,
                completion_tokens INTEGER,
                total_tokens INTEGER,
                cached_tokens INTEGER
             );
             CREATE INDEX IF NOT EXISTS idx_ai_requests_time
             ON ai_requests (started_at DESC, id DESC);
             CREATE INDEX IF NOT EXISTS idx_ai_requests_usage
             ON ai_requests (started_at, outcome, prompt_tokens, completion_tokens,
                             total_tokens, cached_tokens);",
        )?;
        Ok(Self { pool })
    }

    pub(crate) fn insert(&self, record: &RequestRecord) -> Result<()> {
        self.pool.get()?.execute(
            "INSERT INTO ai_requests (
                started_at, provider_type, provider_name, model_name, model, operation,
                outcome, http_status, elapsed_ms, prompt_tokens, completion_tokens,
                total_tokens, cached_tokens
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                record.started_at,
                record.provider_type,
                record.provider_name,
                record.model_name,
                record.model,
                record.operation,
                record.outcome,
                record.http_status,
                record.elapsed_ms,
                record.usage.prompt_tokens,
                record.usage.completion_tokens,
                record.usage.total_tokens,
                record.usage.cached_tokens,
            ],
        )?;
        Ok(())
    }

    pub fn query(
        &self,
        start: i64,
        end: i64,
        cursor: Option<(i64, i64)>,
        limit: u32,
        include_summary: bool,
    ) -> Result<RequestStatisticsPage> {
        let mut connection = self.pool.get()?;
        let transaction = connection.transaction()?;
        let summary = if include_summary {
            Some(transaction.query_row(
                "SELECT COUNT(*),
                    COUNT(CASE WHEN outcome = 'success' THEN 1 END),
                    COUNT(CASE WHEN outcome = 'failed' THEN 1 END),
                    COUNT(CASE WHEN outcome = 'interrupted' THEN 1 END),
                    COUNT(CASE WHEN prompt_tokens IS NOT NULL OR completion_tokens IS NOT NULL
                        OR total_tokens IS NOT NULL OR cached_tokens IS NOT NULL THEN 1 END),
                    SUM(prompt_tokens), SUM(completion_tokens), SUM(total_tokens), SUM(cached_tokens),
                    SUM(CASE WHEN cached_tokens IS NOT NULL AND prompt_tokens > 0 THEN cached_tokens END),
                    SUM(CASE WHEN cached_tokens IS NOT NULL AND prompt_tokens > 0 THEN prompt_tokens END)
                 FROM ai_requests WHERE started_at >= ?1 AND started_at < ?2",
                params![start, end],
                |row| {
                    let cache_hits: Option<f64> = row.get(9)?;
                    let cache_inputs: Option<f64> = row.get(10)?;
                    Ok(RequestSummary {
                        requests: row.get(0)?,
                        successful: row.get(1)?,
                        failed: row.get(2)?,
                        interrupted: row.get(3)?,
                        usage_reported: row.get(4)?,
                        usage: ChatUsage {
                            prompt_tokens: row.get(5)?,
                            completion_tokens: row.get(6)?,
                            total_tokens: row.get(7)?,
                            cached_tokens: row.get(8)?,
                        },
                        cache_hit_rate: cache_hits.zip(cache_inputs)
                            .filter(|(_, inputs)| *inputs > 0.0)
                            .map(|(hits, inputs)| hits / inputs),
                    })
                },
            )?)
        } else {
            None
        };
        let limit = limit.clamp(1, 100);
        let (cursor_time, cursor_id) = cursor.unwrap_or((i64::MAX, i64::MAX));
        let mut statement = transaction.prepare(
            "SELECT id, started_at, provider_type, provider_name, model_name, model, operation,
                    outcome, http_status, elapsed_ms, prompt_tokens, completion_tokens,
                    total_tokens, cached_tokens
             FROM ai_requests
             WHERE started_at >= ?1 AND started_at < ?2
               AND (started_at, id) < (?3, ?4)
             ORDER BY started_at DESC, id DESC LIMIT ?5",
        )?;
        let mut items = statement
            .query_map(
                params![start, end, cursor_time, cursor_id, limit + 1],
                |row| {
                    Ok(RequestRecord {
                        id: row.get(0)?,
                        started_at: row.get(1)?,
                        provider_type: row.get(2)?,
                        provider_name: row.get(3)?,
                        model_name: row.get(4)?,
                        model: row.get(5)?,
                        operation: row.get(6)?,
                        outcome: row.get(7)?,
                        http_status: row.get(8)?,
                        elapsed_ms: row.get(9)?,
                        usage: ChatUsage {
                            prompt_tokens: row.get(10)?,
                            completion_tokens: row.get(11)?,
                            total_tokens: row.get(12)?,
                            cached_tokens: row.get(13)?,
                        },
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let next_cursor = if items.len() > limit as usize {
            items.pop();
            items
                .last()
                .map(|row| format!("{}:{}", row.started_at, row.id))
        } else {
            None
        };
        drop(statement);
        transaction.commit()?;
        Ok(RequestStatisticsPage {
            items,
            summary,
            next_cursor,
        })
    }
}
