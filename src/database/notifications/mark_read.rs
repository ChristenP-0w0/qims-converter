//! `POST /notifications/read` — mark all of the caller's notifications read
//! (the unread badge clears when the panel is opened; items stay listed).

use axum::{Json, extract::State, http::HeaderMap};
use serde_json::{Value as JsonValue, json};

use crate::database::Database;
use crate::database::users;
use crate::error::AppError;

pub async fn mark_read(
    State(db): State<Database>,
    headers: HeaderMap,
) -> Result<Json<JsonValue>, AppError> {
    let Some(email) = users::caller_email(&headers) else {
        return Ok(Json(json!({ "ok": true })));
    };

    crate::db_timed!(
        "mark notifications read",
        db.query(
            "UPDATE notification SET read = true \
             WHERE recipient = $email AND read = false",
        )
        .bind(("email", email))
    )?;
    Ok(Json(json!({ "ok": true })))
}
