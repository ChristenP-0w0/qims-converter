//! `POST /documents/{id}/submit` — move a Draft to "Under Review".

use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use surrealdb::types::Value;

use crate::database::Database;
use crate::error::AppError;

#[derive(Debug, Default, Deserialize)]
pub struct SubmitInput {
    /// Who submitted (auto-filled from the signed-in user).
    #[serde(default)]
    pub actor: String,
}

pub async fn submit(
    State(db): State<Database>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Option<Json<SubmitInput>>,
) -> Result<Json<JsonValue>, AppError> {
    crate::database::users::require_writer(&db, &headers).await?;
    let actor = body.map(|Json(b)| b.actor).unwrap_or_default();
    log::info!("submitting document:{} for review by {}", id, actor);

    let now = chrono::Utc::now().to_rfc3339();
    let mut res = crate::db_timed!(
        "submit document",
        db.query(
            "UPDATE type::record('document', $id) SET \
             status = 'Under Review', updated_at = $updated, \
             events = array::append(events ?? [], { \
               kind: 'submitted', actor: $actor, detail: '', sections: [], at: $updated \
             }) RETURN AFTER",
        )
        .bind(("id", id))
        .bind(("updated", now))
        .bind(("actor", actor))
    )?;
    let updated: Vec<Value> = res.take(0)?;
    match updated.into_iter().next() {
        Some(v) => Ok(Json(v.into_json_value())),
        None => Err(AppError::NotFound),
    }
}
