use super::error::ApiError;
use super::state::AdminState;
use crate::chat_history::load_chat_history_context;
use crate::history_compression::render_summary_date_heading;
use crate::memory::{GroupMemorySession, MAX_RETENTION_DAYS, SECONDS_PER_DAY};
use crate::repository::db_manager::ConversationRecord;
use crate::runtime_state::RuntimeGroupMember;
use crate::transport::message::{
    preferred_sender_name, Conversation, ConversationKind, MessageTarget,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, put},
    Json, Router,
};
use chrono::{Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};

const GROUP_MEMBER_CACHE_TTL: Duration = Duration::from_secs(10 * 60);

pub(super) fn routes() -> Router<Arc<AdminState>> {
    Router::new()
        .route("/conversations", get(conversations))
        .route("/conversations/{conversation_id}", get(conversation_detail))
        .route(
            "/conversations/{conversation_id}/messages",
            get(conversation_messages),
        )
        .route(
            "/conversations/{conversation_id}/user-memories",
            get(user_memories).post(create_user_memory),
        )
        .route(
            "/conversations/{conversation_id}/users/{user_id}/memories/{memory_id}",
            put(update_user_memory).delete(delete_user_memory),
        )
        .route(
            "/conversations/{conversation_id}/group-memories",
            get(group_memories).post(create_group_memory),
        )
        .route(
            "/conversations/{conversation_id}/group-memories/{memory_id}",
            put(update_group_memory).delete(delete_group_memory),
        )
        .route(
            "/conversations/{conversation_id}/scheduled-tasks",
            get(scheduled_tasks).post(create_scheduled_task),
        )
        .route(
            "/conversations/{conversation_id}/scheduled-tasks/{task_id}",
            put(update_scheduled_task).delete(delete_scheduled_task),
        )
}

#[derive(Deserialize)]
struct ConversationListQuery {
    kind: Option<String>,
    cursor: Option<String>,
    limit: Option<u32>,
}

#[derive(Serialize)]
struct ConversationSummaryResponse {
    id: i64,
    source_id: String,
    kind: String,
    title: String,
    member_count: Option<u64>,
    latest_sender_name: Option<String>,
    latest_content: Option<String>,
    latest_message_at: Option<i64>,
    unread_count: u64,
}

async fn conversations(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<ConversationListQuery>,
) -> Result<Json<Value>, ApiError> {
    let cursor = query.cursor.as_deref().map(parse_cursor).transpose()?;
    let rows = state.db_manager.list_admin_conversations(
        query.kind.as_deref().filter(|kind| *kind != "all"),
        cursor,
        query.limit.unwrap_or(30),
    )?;
    let requested_limit = query.limit.unwrap_or(30).clamp(1, 100) as usize;
    let next_cursor = (rows.len() == requested_limit)
        .then(|| {
            rows.last().map(|row| {
                format!(
                    "{}:{}",
                    row.conversation.last_message_at, row.conversation.id
                )
            })
        })
        .flatten();
    let items = rows
        .into_iter()
        .map(|row| {
            let group = (row.conversation.kind == "group")
                .then(|| {
                    state
                        .runtime
                        .group(&row.conversation.source_conversation_id)
                })
                .flatten();
            ConversationSummaryResponse {
                id: row.conversation.id,
                source_id: row.conversation.source_conversation_id.clone(),
                kind: row.conversation.kind,
                title: group
                    .as_ref()
                    .map(|group| group.name.clone())
                    .or(row.conversation.title)
                    .unwrap_or(row.conversation.source_conversation_id),
                member_count: group.map(|group| group.member_count),
                latest_sender_name: row.latest_sender_name,
                latest_content: row.latest_content,
                latest_message_at: row.latest_message_at,
                unread_count: row.unread_count,
            }
        })
        .collect::<Vec<_>>();
    let stats = state.db_manager.admin_database_stats(0)?;
    Ok(Json(json!({
        "items": items,
        "next_cursor": next_cursor,
        "counts": {
            "all": stats.conversations,
            "group": stats.group_conversations,
            "direct": stats.direct_conversations,
        }
    })))
}

fn parse_cursor(cursor: &str) -> Result<(i64, i64), ApiError> {
    let (timestamp, id) = cursor
        .split_once(':')
        .ok_or_else(|| ApiError::bad_request("会话游标无效"))?;
    Ok((timestamp.parse()?, id.parse()?))
}

async fn conversation_detail(
    State(state): State<Arc<AdminState>>,
    Path(conversation_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    let group = (conversation.kind == "group")
        .then(|| state.runtime.group(&conversation.source_conversation_id))
        .flatten();
    Ok(Json(json!({
        "id": conversation.id,
        "source": conversation.source,
        "source_id": conversation.source_conversation_id,
        "kind": conversation.kind,
        "title": group.as_ref().map(|value| value.name.clone()).or(conversation.title),
        "member_count": group.as_ref().map(|value| value.member_count),
        "max_member_count": group.and_then(|value| value.max_member_count),
        "last_message_at": conversation.last_message_at,
    })))
}

async fn conversation_messages(
    State(state): State<Arc<AdminState>>,
    Path(conversation_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    let bot_id = state.runtime.bot_id();
    let bot_name = state.runtime.bot_name();
    let history = load_chat_history_context(
        &state.db_manager,
        &conversation.source,
        &conversation.source_conversation_id,
        if conversation.kind == "group" {
            state.group_context_history_limit
        } else {
            state.direct_context_history_limit
        },
        state.context_summary_enabled,
        state.context_summary_days,
    )?;
    let summaries = history
        .summaries
        .iter()
        .map(|summary| {
            Ok(json!({
                "date": summary.summary_date,
                "heading": render_summary_date_heading(&summary.summary_date)?,
                "content": summary.summary_text,
            }))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let items = history.window.messages
        .into_iter()
        .map(|message| {
            let is_bot = bot_id.as_deref() == Some(message.sender_id.as_str());
            let sender_name = if is_bot {
                bot_name
                    .as_deref()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or(&message.sender_display_name)
                    .to_string()
            } else {
                preferred_sender_name(
                    &message.sender_display_name,
                    message.sender_nickname.as_deref(),
                )
                .to_string()
            };
            json!({
                "id": message.id,
                "sender_id": message.sender_id,
                "sender_name": sender_name,
                "content": message.content_text.unwrap_or_default(),
                "parts": serde_json::from_str::<Value>(&message.content_parts_json).unwrap_or(Value::Array(Vec::new())),
                "is_bot": is_bot,
                "is_read": message.is_read,
                "timestamp": message.event_timestamp,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "items": items, "summaries": summaries })))
}

#[derive(Serialize)]
struct MemberMemoryResponse {
    user_id: String,
    nickname: String,
    card: String,
    role: String,
    memories: Vec<Value>,
}

async fn user_memories(
    State(state): State<Arc<AdminState>>,
    Path(conversation_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    let (members, stale) = conversation_members(&state, &conversation).await?;
    let bot_id = require_bot_id(&state)?;
    let user_ids = members
        .iter()
        .map(|member| member.user_id.clone())
        .collect::<Vec<_>>();
    let memories =
        state
            .db_manager
            .get_user_memories_for_users(&conversation.source, &bot_id, &user_ids)?;
    let mut by_user = HashMap::<String, Vec<Value>>::new();
    for memory in memories {
        by_user.entry(memory.user_id).or_default().push(json!({
            "id": memory.memory_id,
            "content": memory.content,
            "updated_date": crate::memory::format_memory_time(memory.updated_at)?,
        }));
    }
    let users = members
        .into_iter()
        .map(|member| MemberMemoryResponse {
            memories: by_user.remove(&member.user_id).unwrap_or_default(),
            user_id: member.user_id,
            nickname: member.nickname,
            card: member.card,
            role: member.role,
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "users": users, "stale": stale })))
}

#[derive(Deserialize)]
struct UserMemoryCreate {
    user_id: String,
    content: String,
}

async fn create_user_memory(
    State(state): State<Arc<AdminState>>,
    Path(conversation_id): Path<i64>,
    Json(request): Json<UserMemoryCreate>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    ensure_conversation_member(&state, &conversation, &request.user_id).await?;
    let memory_id = state.user_memory.create(
        &conversation.source,
        &require_bot_id(&state)?,
        &request.user_id,
        &request.content,
    )?;
    Ok((StatusCode::CREATED, Json(json!({ "id": memory_id }))))
}

#[derive(Deserialize)]
struct UserMemoryUpdate {
    content: String,
}

async fn update_user_memory(
    State(state): State<Arc<AdminState>>,
    Path((conversation_id, user_id, memory_id)): Path<(i64, String, String)>,
    Json(request): Json<UserMemoryUpdate>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    ensure_conversation_member(&state, &conversation, &user_id).await?;
    state.user_memory.update(
        &conversation.source,
        &require_bot_id(&state)?,
        &user_id,
        &memory_id,
        &request.content,
    )?;
    Ok(Json(json!({ "updated": true })))
}

async fn delete_user_memory(
    State(state): State<Arc<AdminState>>,
    Path((conversation_id, user_id, memory_id)): Path<(i64, String, String)>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    state.user_memory.delete(
        &conversation.source,
        &require_bot_id(&state)?,
        &user_id,
        &memory_id,
    )?;
    Ok(Json(json!({ "deleted": true })))
}

async fn group_memories(
    State(state): State<Arc<AdminState>>,
    Path(conversation_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    let records = state.db_manager.get_group_memories(
        &conversation.source,
        &require_bot_id(&state)?,
        &conversation.source_conversation_id,
    )?;
    let now = Utc::now().timestamp();
    let items = records.into_iter().map(|memory| json!({
        "id": memory.id,
        "title": memory.title,
        "content": memory.content,
        "expires_at": memory.expires_at,
        "updated_at": memory.updated_at,
        "updated_date": chrono::DateTime::from_timestamp(memory.updated_at, 0).map(|time| time.with_timezone(&Local).format("%Y-%m-%d").to_string()),
        "permanent": memory.expires_at == 0,
        "remaining_days": ((memory.expires_at - now).max(0) + SECONDS_PER_DAY - 1) / SECONDS_PER_DAY,
        "expiring": memory.expires_at != 0 && memory.expires_at <= now,
    })).collect::<Vec<_>>();
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
struct GroupMemoryWrite {
    title: String,
    content: String,
    retention_days: u16,
}

async fn create_group_memory(
    State(state): State<Arc<AdminState>>,
    Path(conversation_id): Path<i64>,
    Json(request): Json<GroupMemoryWrite>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    let session = GroupMemorySession::new(
        message_target(&state, &conversation)?,
        state.db_manager.clone(),
    );
    let result = session.set_memory(
        &request.title,
        Some(&request.content),
        Some(request.retention_days),
    )?;
    Ok((StatusCode::CREATED, Json(json!({ "message": result }))))
}

async fn update_group_memory(
    State(state): State<Arc<AdminState>>,
    Path((conversation_id, memory_id)): Path<(i64, i64)>,
    Json(request): Json<GroupMemoryWrite>,
) -> Result<Json<Value>, ApiError> {
    GroupMemorySession::validate_title(request.title.trim())?;
    GroupMemorySession::validate_content(request.content.trim())?;
    if request.retention_days > MAX_RETENTION_DAYS {
        return Err(ApiError::bad_request(
            "群记忆时间必须在 0 到 365 天之间，0 表示永不过期",
        ));
    }
    let conversation = get_conversation(&state, conversation_id)?;
    let expires_at = if request.retention_days == 0 {
        0
    } else {
        Utc::now().timestamp() + i64::from(request.retention_days) * SECONDS_PER_DAY
    };
    let updated = state.db_manager.update_group_memory_by_id(
        &conversation.source,
        &require_bot_id(&state)?,
        &conversation.source_conversation_id,
        memory_id,
        request.title.trim(),
        request.content.trim(),
        expires_at,
    )?;
    if !updated {
        return Err(ApiError::not_found("群记忆不存在"));
    }
    Ok(Json(json!({ "updated": true })))
}

async fn delete_group_memory(
    State(state): State<Arc<AdminState>>,
    Path((conversation_id, memory_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    let deleted = state.db_manager.delete_group_memory_by_id(
        &conversation.source,
        &require_bot_id(&state)?,
        &conversation.source_conversation_id,
        memory_id,
    )?;
    if !deleted {
        return Err(ApiError::not_found("群记忆不存在"));
    }
    Ok(Json(json!({ "deleted": true })))
}

async fn scheduled_tasks(
    State(state): State<Arc<AdminState>>,
    Path(conversation_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    let target = message_target(&state, &conversation)?;
    let items = state
        .scheduler
        .running_tasks(&target)?
        .into_iter()
        .map(|task| {
            json!({
                "id": task.id,
                "title": task.title,
                "schedule": task.schedule,
                "instruction": task.instruction,
                "next_run_at": task.next_run_at,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
struct ScheduledTaskWrite {
    title: String,
    schedule: String,
    instruction: String,
}

async fn create_scheduled_task(
    State(state): State<Arc<AdminState>>,
    Path(conversation_id): Path<i64>,
    Json(request): Json<ScheduledTaskWrite>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    let task = state.scheduler.create_task(
        &message_target(&state, &conversation)?,
        &request.title,
        &request.schedule,
        &request.instruction,
    )?;
    Ok((StatusCode::CREATED, Json(json!({ "id": task.id }))))
}

async fn update_scheduled_task(
    State(state): State<Arc<AdminState>>,
    Path((conversation_id, task_id)): Path<(i64, String)>,
    Json(request): Json<ScheduledTaskWrite>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    state.scheduler.update_task(
        &message_target(&state, &conversation)?,
        &task_id,
        Some(&request.title),
        Some(&request.schedule),
        Some(&request.instruction),
    )?;
    Ok(Json(json!({ "updated": true })))
}

async fn delete_scheduled_task(
    State(state): State<Arc<AdminState>>,
    Path((conversation_id, task_id)): Path<(i64, String)>,
) -> Result<Json<Value>, ApiError> {
    let conversation = get_conversation(&state, conversation_id)?;
    let deleted = state
        .scheduler
        .delete_task(&message_target(&state, &conversation)?, &task_id)?;
    if !deleted {
        return Err(ApiError::not_found("定时任务不存在"));
    }
    Ok(Json(json!({ "deleted": true })))
}

fn get_conversation(
    state: &AdminState,
    conversation_id: i64,
) -> Result<ConversationRecord, ApiError> {
    state
        .db_manager
        .get_conversation_by_id(conversation_id)?
        .ok_or_else(|| ApiError::not_found("会话不存在"))
}

fn require_bot_id(state: &AdminState) -> Result<String, ApiError> {
    state
        .runtime
        .bot_id()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "尚未获取 Bot QQ 号"))
}

fn message_target(
    state: &AdminState,
    conversation: &ConversationRecord,
) -> Result<MessageTarget, ApiError> {
    let kind = match conversation.kind.as_str() {
        "group" => ConversationKind::Group,
        "direct" => ConversationKind::Direct,
        _ => return Err(ApiError::bad_request("会话类型无效")),
    };
    Ok(MessageTarget {
        source: conversation.source.clone(),
        bot_id: require_bot_id(state)?,
        conversation: Conversation {
            id: conversation.source_conversation_id.clone(),
            kind,
            title: conversation.title.clone(),
        },
    })
}

async fn conversation_members(
    state: &AdminState,
    conversation: &ConversationRecord,
) -> Result<(Vec<RuntimeGroupMember>, bool), ApiError> {
    if conversation.kind == "direct" {
        return Ok((
            vec![RuntimeGroupMember {
                user_id: conversation.source_conversation_id.clone(),
                nickname: conversation
                    .title
                    .clone()
                    .unwrap_or_else(|| conversation.source_conversation_id.clone()),
                card: String::new(),
                role: "friend".to_string(),
            }],
            false,
        ));
    }
    let group_id = conversation.source_conversation_id.parse::<i64>()?;
    let cached = state
        .runtime
        .cached_group_members(&conversation.source_conversation_id);
    if cached
        .as_ref()
        .is_some_and(|entry| entry.is_fresh(GROUP_MEMBER_CACHE_TTL))
    {
        return Ok((cached.unwrap().members, false));
    }
    match state.onebot.fetch_group_members(group_id).await {
        Ok(mut members) => {
            if let Some(bot_id) = state.runtime.bot_id() {
                members.retain(|member| member.user_id != bot_id);
            }
            state
                .runtime
                .cache_group_members(&conversation.source_conversation_id, members.clone());
            Ok((members, false))
        }
        Err(error) => cached.map(|entry| (entry.members, true)).ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("获取群成员失败：{:#}", error),
            )
        }),
    }
}

async fn ensure_conversation_member(
    state: &AdminState,
    conversation: &ConversationRecord,
    user_id: &str,
) -> Result<(), ApiError> {
    let (members, _) = conversation_members(state, conversation).await?;
    if members
        .iter()
        .any(|member| member.user_id == user_id.trim())
    {
        Ok(())
    } else {
        Err(ApiError::bad_request("指定 QQ 号不是当前会话成员"))
    }
}
