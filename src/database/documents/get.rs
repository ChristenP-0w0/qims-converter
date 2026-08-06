//! `GET /documents/{id}` — fetch a single document by its record id.

use axum::{
    Json,
    extract::{Path, State},
};
use serde_json::Value as JsonValue;
use surrealdb::types::Value;

use crate::database::Database;
use crate::error::AppError;

pub async fn get(
    State(db): State<Database>,
    Path(id): Path<String>,
) -> Result<Json<JsonValue>, AppError> {
    let mut res = crate::db_timed!(
        "get document",
        db.query("SELECT * FROM type::record('document', $id)")
            .bind(("id", id))
    )?;
    let found: Vec<Value> = res.take(0)?;
    match found.into_iter().next() {
        Some(v) => Ok(Json(v.into_json_value())),
        None => Err(AppError::NotFound),
    }
}
