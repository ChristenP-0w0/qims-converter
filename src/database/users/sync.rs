//! `POST /users/sync` — record the signed-in person in the `user` table.
//!
//! Called by the frontend whenever a session loads. Creates the person on
//! first sign-in (as a viewer — the bootstrap admin as admin) and refreshes
//! the profile and last-seen fields afterwards. Existing users' roles are
//! never touched here: they belong to the admins.

use axum::{Json, extract::State, http::HeaderMap};
use serde_json::Value as JsonValue;
use surrealdb::types::Value;

use super::model::SyncUser;
use crate::database::Database;
use crate::error::AppError;

pub async fn sync(
    State(db): State<Database>,
    headers: HeaderMap,
    Json(input): Json<SyncUser>,
) -> Result<Json<JsonValue>, AppError> {
    // The proxy-asserted identity wins; the body's email is only used when no
    // proxy is in play (local development / AUTH_DISABLED).
    let email = super::caller_email(&headers)
        .unwrap_or_else(|| input.email.trim().to_ascii_lowercase());
    if email.is_empty() {
        return Err(AppError::BadRequest("email is required".into()));
    }

    // The bootstrap admin is pinned to the admin role on every sync; everyone
    // else keeps their assigned role (defaulting to viewer on first sign-in).
    let role_clause = if email == super::bootstrap_admin() {
        "role = 'admin'"
    } else {
        "role = role ?? 'viewer'"
    };
    let sql = format!(
        "UPSERT user SET \
           email = $email, name = $name, first_name = $first_name, \
           last_name = $last_name, username = $username, \
           main_api_id = $main_api_id, \
           first_seen_at = first_seen_at ?? $now, last_seen_at = $now, \
           {role_clause} \
         WHERE email = $email RETURN AFTER"
    );

    let name = if input.name.trim().is_empty() {
        email.clone()
    } else {
        input.name.trim().to_string()
    };
    let now = chrono::Utc::now().to_rfc3339();
    let mut res = crate::db_timed!(
        "sync user",
        db.query(sql.as_str())
            .bind(("email", email))
            .bind(("name", name))
            .bind(("first_name", input.first_name))
            .bind(("last_name", input.last_name))
            .bind(("username", input.username))
            .bind(("main_api_id", input.main_api_id))
            .bind(("now", now))
    )?;
    let rows: Vec<Value> = res.take(0)?;
    let record = rows
        .into_iter()
        .next()
        .map(Value::into_json_value)
        .unwrap_or(JsonValue::Null);
    Ok(Json(record))
}
