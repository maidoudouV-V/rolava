use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;

use super::error::ApiError;
use super::state::AdminState;

pub(super) async fn require_admin(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let app_config = state.app_config();
    if bearer_token(&headers)
        .is_some_and(|token| constant_time_equals(token, &app_config.admin.token))
    {
        return next.run(request).await;
    }
    ApiError::new(StatusCode::UNAUTHORIZED, "管理 Token 无效").into_response()
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn constant_time_equals(actual: &str, expected: &str) -> bool {
    actual.len() == expected.len() && actual.as_bytes().ct_eq(expected.as_bytes()).into()
}

pub(super) async fn verify_auth() -> Json<Value> {
    Json(json!({ "authenticated": true }))
}
