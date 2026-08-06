//! `POST /documents` — validate and persist a new document (always a Draft).

use axum::{Json, extract::State, http::HeaderMap};
use serde_json::Value as JsonValue;
use surrealdb::types::Value;

use super::model::{CreateDocument, Event};
use crate::database::Database;
use crate::database::users;
use crate::error::AppError;

pub async fn create(
    State(db): State<Database>,
    headers: HeaderMap,
    Json(mut input): Json<CreateDocument>,
) -> Result<Json<JsonValue>, AppError> {
    users::require_writer(&db, &headers).await?;

    // Server-controlled fields: a new document is always a Draft with no
    // approvals, and its timestamps are set here. The author's email comes
    // from the session identity, never the payload.
    input.author_email = users::caller_email(&headers).unwrap_or_default();
    input.status = "Draft".to_string();
    input.approved_by = Vec::new();
    let now = chrono::Utc::now().to_rfc3339();
    input.created_at = now.clone();
    input.updated_at = now.clone();
    input.events = vec![Event {
        kind: "created".to_string(),
        actor: input.author.clone(),
        detail: String::new(),
        sections: Vec::new(),
        at: now,
    }];

    input.validate()?;

    log::info!(
        "creating {} '{}' ({}, {})",
        input.kind,
        input.title,
        input.document_number,
        input.document_type
    );

    let mut res = crate::db_timed!(
        "create document",
        db.query("CREATE document CONTENT $data").bind(("data", input))
    )?;
    let created: Vec<Value> = res.take(0)?;
    let record = created
        .into_iter()
        .next()
        .map(Value::into_json_value)
        .unwrap_or(JsonValue::Null);
    Ok(Json(record))
}
