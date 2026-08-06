//! `DELETE /notifications` / `DELETE /notifications/{id}` — clear all of the
//! caller's notifications (the sweep button) or a single one (the ✕ button).
//! Ownership is enforced in the query: you can only delete what's yours.

use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use serde_json::{Value as JsonValue, json};

use crate::database::Database;
use crate::database::users;
use crate::error::AppError;

pub async fn clear_all(
    State(db): State<Database>,
    headers: HeaderMap,
) -> Result<Json<JsonValue>, AppError> {
    let Some(email) = users::caller_email(&headers) else {
        return Ok(Json(json!({ "ok": true })));
    };
    crate::db_timed!(
        "clear notifications",
        db.query("DELETE notification WHERE recipient = $email")
            .bind(("email", email))
    )?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn clear_one(
    State(db): State<Database>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<JsonValue>, AppError> {
    let Some(email) = users::caller_email(&headers) else {
        return Ok(Json(json!({ "ok": true })));
    };
    crate::db_timed!(
        "clear notification",
        db.query(
            "DELETE type::record('notification', $id) WHERE recipient = $email",
        )
        .bind(("id", id))
        .bind(("email", email))
    )?;
    Ok(Json(json!({ "ok": true })))
}
