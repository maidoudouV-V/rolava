use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use super::error::ApiError;
use super::state::AdminState;

#[derive(Deserialize)]
pub(super) struct StatisticsQuery {
    start: Option<i64>,
    end: Option<i64>,
    cursor: Option<String>,
    limit: Option<u32>,
    summary: Option<bool>,
}

pub(super) async fn get_statistics(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<StatisticsQuery>,
) -> Result<Json<Value>, ApiError> {
    let end = query.end.unwrap_or_else(|| Utc::now().timestamp() + 1);
    let start = query
        .start
        .unwrap_or_else(|| end.saturating_sub(86400).max(0));
    if start < 0 || end <= start || end > 253402300799 {
        return Err(ApiError::bad_request(
            "请选择有效的开始与结束时间，开始时间必须早于结束时间",
        ));
    }
    let cursor = query
        .cursor
        .as_deref()
        .map(|cursor| {
            let (time, id) = cursor
                .split_once(':')
                .ok_or_else(|| ApiError::bad_request("请求统计游标无效"))?;
            let time = time.parse::<i64>().ok().filter(|value| *value >= 0);
            let id = id.parse::<i64>().ok().filter(|value| *value > 0);
            time.zip(id)
                .ok_or_else(|| ApiError::bad_request("请求统计游标无效"))
        })
        .transpose()?;
    let store = state.request_statistics.clone();
    let page = tokio::task::spawn_blocking(move || {
        store.query(
            start,
            end,
            cursor,
            query.limit.unwrap_or(50),
            query.summary.unwrap_or(true),
        )
    })
    .await??;
    Ok(Json(json!({
        "items": page.items,
        "summary": page.summary,
        "next_cursor": page.next_cursor,
        "start": start,
        "end": end,
    })))
}
