//! Application error type and its HTTP representation.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};

/// Any error that can be turned into an HTTP response.
pub enum AppError {
    /// A database/query failure — becomes `500`.
    Db(surrealdb::Error),
    /// Invalid client input — becomes `400`.
    BadRequest(String),
    /// The caller's role does not permit the action — becomes `403`.
    Forbidden(String),
    /// Requested resource does not exist — becomes `404`.
    NotFound,
    /// An unexpected server-side failure — becomes `500`.
    Internal(String),
}

impl From<surrealdb::Error> for AppError {
    fn from(e: surrealdb::Error) -> Self {
        AppError::Db(e)
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::Db(e) => write!(f, "database error: {e}"),
            AppError::BadRequest(m) => write!(f, "bad request: {m}"),
            AppError::Forbidden(m) => write!(f, "forbidden: {m}"),
            AppError::NotFound => write!(f, "not found"),
            AppError::Internal(m) => write!(f, "{m}"),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            AppError::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            AppError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            AppError::Internal(m) => {
                log::error!("internal error: {m}");
                (StatusCode::INTERNAL_SERVER_ERROR, m)
            }
            AppError::Db(e) => {
                log::error!("database error: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            }
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}
