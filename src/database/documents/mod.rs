//! The `document` resource: model, validation, and operations.
//!
//! Operations are split one-per-file (`create`, `list`), and the HTTP routes
//! for the resource are assembled by [`routes`].

pub mod create;
pub mod get;
pub mod history;
pub mod list;
pub mod model;
pub mod original;
pub mod review;
pub mod submit;
pub mod update;

use axum::{
    Router,
    routing::{get as get_method, post},
};

use crate::database::Database;

/// Ensure the `document` table exists so reads succeed on a fresh database.
/// Without this, `SELECT * FROM document` errors until the first record is
/// created.
pub async fn init(db: &Database) -> Result<(), surrealdb::Error> {
    crate::db_timed!(
        "define document table",
        db.query("DEFINE TABLE IF NOT EXISTS document SCHEMALESS")
    )?
    .check()?;
    Ok(())
}

/// Routes for the documents resource, mounted at the application root.
pub fn routes() -> Router<Database> {
    Router::new()
        .route("/documents", post(create::create).get(list::list))
        .route(
            "/documents/{id}",
            get_method(get::get).put(update::update),
        )
        .route("/documents/{id}/history", get_method(history::history))
        .route("/documents/{id}/original", get_method(original::original))
        .route("/documents/{id}/submit", post(submit::submit))
        .route("/documents/{id}/review", post(review::review))
}
