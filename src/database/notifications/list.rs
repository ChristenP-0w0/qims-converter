//! `GET /notifications` — the caller's notifications, newest first.

use axum::{Json, extract::State, http::HeaderMap};
use serde_json::Value as JsonValue;
use surrealdb::types::Value;

use crate::database::Database;
use crate::database::users;
use crate::error::AppError;

pub async fn list(
    State(db): State<Database>,
    headers: HeaderMap,
) -> Result<Json<JsonValue>, AppError> {
    // No proxy identity (local dev) → nothing is addressed to you.
    let Some(email) = users::caller_email(&headers) else {
        return Ok(Json(JsonValue::Array(Vec::new())));
    };

    let mut res = crate::db_timed!(
        "list notifications",
        db.query(
            "SELECT * FROM notification WHERE recipient = $email \
             ORDER BY created_at DESC LIMIT 50",
        )
        .bind(("email", email))
    )?;
    let rows: Vec<Value> = res.take(0)?;
    Ok(Json(JsonValue::Array(
        rows.into_iter().map(Value::into_json_value).collect(),
    )))
}
