//! `GET /users` — everyone who has signed in to QIMS (admin only).

use axum::{Json, extract::State, http::HeaderMap};
use serde_json::Value as JsonValue;
use surrealdb::types::Value;

use crate::database::Database;
use crate::error::AppError;

pub async fn list(
    State(db): State<Database>,
    headers: HeaderMap,
) -> Result<Json<JsonValue>, AppError> {
    super::require_admin(&db, &headers).await?;

    let mut res = crate::db_timed!(
        "list users",
        db.query("SELECT * FROM user ORDER BY name ASC")
    )?;
    let rows: Vec<Value> = res.take(0)?;
    let bootstrap = super::bootstrap_admin();
    let users = rows
        .into_iter()
        .map(|v| {
            let mut user = v.into_json_value();
            // Flag the permanent administrator so the UI can lock their role.
            let permanent = user
                .get("email")
                .and_then(JsonValue::as_str)
                .is_some_and(|e| e == bootstrap);
            if let Some(obj) = user.as_object_mut() {
                obj.insert("permanent".into(), JsonValue::Bool(permanent));
            }
            user
        })
        .collect();
    Ok(Json(JsonValue::Array(users)))
}
