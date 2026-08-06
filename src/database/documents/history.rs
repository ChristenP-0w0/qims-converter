//! `GET /documents/{id}/history` — the activity log for a document *number*:
//! events from every edition/revision sharing the number, merged
//! chronologically and tagged with their edition, plus the earliest creation
//! and latest edit timestamps across the whole number.

use axum::{
    Json,
    extract::{Path, State},
};
use serde_json::{Value as JsonValue, json};
use surrealdb::types::Value;

use crate::database::Database;
use crate::error::AppError;

pub async fn history(
    State(db): State<Database>,
    Path(id): Path<String>,
) -> Result<Json<JsonValue>, AppError> {
    // Which number does this document belong to?
    let mut res = crate::db_timed!(
        "resolve document number",
        db.query("SELECT document_number FROM type::record('document', $id)")
            .bind(("id", id.clone()))
    )?;
    let rows: Vec<Value> = res.take(0)?;
    let current = match rows.into_iter().next() {
        Some(v) => v.into_json_value(),
        None => return Err(AppError::NotFound),
    };
    let number = current
        .get("document_number")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .to_string();

    // Every edition sharing the number — or just this record when unnumbered
    // (an empty number must not merge unrelated documents).
    let mut res2 = if number.trim().is_empty() {
        crate::db_timed!(
            "collect record history",
            db.query(
                "SELECT edition, created_at, updated_at, events \
                 FROM type::record('document', $id)",
            )
            .bind(("id", id))
        )?
    } else {
        crate::db_timed!(
            "collect number history",
            db.query(
                "SELECT edition, created_at, updated_at, events \
                 FROM document WHERE document_number = $num",
            )
            .bind(("num", number.clone()))
        )?
    };
    let docs: Vec<Value> = res2.take(0)?;

    let mut events: Vec<JsonValue> = Vec::new();
    let mut created_at: Option<String> = None;
    let mut updated_at: Option<String> = None;
    for doc in docs {
        let doc = doc.into_json_value();
        let edition = doc.get("edition").and_then(JsonValue::as_u64).unwrap_or(0);
        if let Some(c) = doc.get("created_at").and_then(JsonValue::as_str) {
            if created_at.as_deref().is_none_or(|cur| c < cur) {
                created_at = Some(c.to_string());
            }
        }
        if let Some(u) = doc.get("updated_at").and_then(JsonValue::as_str) {
            if updated_at.as_deref().is_none_or(|cur| u > cur) {
                updated_at = Some(u.to_string());
            }
        }
        if let Some(list) = doc.get("events").and_then(JsonValue::as_array) {
            for event in list {
                let mut event = event.clone();
                if let Some(obj) = event.as_object_mut() {
                    obj.insert("edition".to_string(), json!(edition));
                }
                events.push(event);
            }
        }
    }

    // RFC3339 UTC timestamps sort correctly as strings.
    events.sort_by(|a, b| {
        let at_a = a.get("at").and_then(JsonValue::as_str).unwrap_or("");
        let at_b = b.get("at").and_then(JsonValue::as_str).unwrap_or("");
        at_a.cmp(at_b)
    });

    Ok(Json(json!({
        "document_number": number,
        "created_at": created_at,
        "updated_at": updated_at,
        "events": events,
    })))
}
