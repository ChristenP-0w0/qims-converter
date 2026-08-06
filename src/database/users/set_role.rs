//! `PUT /users/{id}/role` — assign a role to a user (admin only).

use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use surrealdb::types::Value;

use super::model::ALLOWED_ROLES;
use crate::database::Database;
use crate::error::AppError;

#[derive(Debug, Deserialize)]
pub struct SetRoleInput {
    /// One of [`ALLOWED_ROLES`] — admin / writer / viewer.
    pub role: String,
}

pub async fn set_role(
    State(db): State<Database>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<SetRoleInput>,
) -> Result<Json<JsonValue>, AppError> {
    super::require_admin(&db, &headers).await?;

    if !ALLOWED_ROLES.contains(&input.role.as_str()) {
        return Err(AppError::BadRequest(format!(
            "invalid role: {}",
            input.role
        )));
    }

    // Who the change applies to — and the one hard rule: the bootstrap admin
    // stays an admin, no matter who asks.
    let mut sel = crate::db_timed!(
        "load user",
        db.query("SELECT VALUE email FROM type::record('user', $id)")
            .bind(("id", id.clone()))
    )?;
    let emails: Vec<Value> = sel.take(0)?;
    let target_email = match emails.into_iter().next() {
        Some(v) => v
            .into_json_value()
            .as_str()
            .unwrap_or_default()
            .to_string(),
        None => return Err(AppError::NotFound),
    };
    if target_email == super::bootstrap_admin() && input.role != "admin" {
        return Err(AppError::BadRequest(format!(
            "{target_email} is the permanent administrator and cannot be demoted"
        )));
    }

    let actor = super::caller_email(&headers).unwrap_or_else(|| "local".to_string());
    log::info!("role of {target_email} set to {} by {actor}", input.role);

    let now = chrono::Utc::now().to_rfc3339();
    let mut res = crate::db_timed!(
        "set user role",
        db.query(
            "UPDATE type::record('user', $id) SET \
             role = $role, role_set_by = $actor, role_set_at = $now RETURN AFTER",
        )
        .bind(("id", id))
        .bind(("role", input.role))
        .bind(("actor", actor))
        .bind(("now", now))
    )?;
    let rows: Vec<Value> = res.take(0)?;
    match rows.into_iter().next() {
        Some(v) => Ok(Json(v.into_json_value())),
        None => Err(AppError::NotFound),
    }
}
