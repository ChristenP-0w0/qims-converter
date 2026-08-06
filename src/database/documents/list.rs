//! `GET /documents` — list all documents, newest first.
//!
//! Returns metadata only: `body` (which can be megabytes of HTML with
//! embedded images) and `page_setup` are OMITted — the listing renders a
//! table, and the full record is fetched per-document when opened.

use axum::{Json, extract::State};
use serde_json::Value as JsonValue;
use surrealdb::types::Value;

use crate::database::Database;
use crate::error::AppError;

pub async fn list(State(db): State<Database>) -> Result<Json<JsonValue>, AppError> {
    let mut res = crate::db_timed!(
        "list documents",
        db.query("SELECT * OMIT body, page_setup FROM document ORDER BY date DESC")
    )?;
    let docs: Vec<Value> = res.take(0)?;
    log::debug!("listing {} document(s)", docs.len());
    let json: Vec<JsonValue> = docs.into_iter().map(Value::into_json_value).collect();
    Ok(Json(JsonValue::Array(json)))
}
