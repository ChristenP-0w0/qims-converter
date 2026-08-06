//! `GET /documents/{id}/original` — download the untouched file a document
//! was imported from (kept in `data/originals`, referenced by `source_file`).

use axum::{
    extract::{Path, State},
    http::header,
    response::{IntoResponse, Response},
};
use serde_json::Value as JsonValue;
use surrealdb::types::Value;

use crate::database::Database;
use crate::error::AppError;

pub async fn original(
    State(db): State<Database>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let mut sel = crate::db_timed!(
        "load document source",
        db.query(
            "SELECT source_file, source_name, source_mime \
             FROM type::record('document', $id)",
        )
        .bind(("id", id))
    )?;
    let rows: Vec<Value> = sel.take(0)?;
    let row = match rows.into_iter().next() {
        Some(v) => v.into_json_value(),
        None => return Err(AppError::NotFound),
    };
    let field = |key: &str| {
        row.get(key)
            .and_then(JsonValue::as_str)
            .unwrap_or("")
            .to_string()
    };
    let file = field("source_file");
    if file.is_empty() {
        return Err(AppError::NotFound);
    }
    // The stored name is server-generated (single path segment), but never
    // trust a value that has been through the database.
    if file.contains('/') || file.contains('\\') || file.contains("..") {
        return Err(AppError::BadRequest("invalid source file".into()));
    }

    let path = crate::convert::originals_dir().join(&file);
    let bytes = tokio::fs::read(&path).await.map_err(|_| {
        log::error!("original missing on disk: {}", path.display());
        AppError::NotFound
    })?;

    let mime = match field("source_mime") {
        m if m.is_empty() => "application/octet-stream".to_string(),
        m => m,
    };
    let name = match field("source_name") {
        n if n.is_empty() => file,
        n => n,
    };
    // Quotes/CR/LF must not break out of the quoted filename.
    let safe_name: String = name
        .chars()
        .map(|c| if matches!(c, '"' | '\r' | '\n') { '_' } else { c })
        .collect();

    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{safe_name}\""),
            ),
        ],
        bytes,
    )
        .into_response())
}
