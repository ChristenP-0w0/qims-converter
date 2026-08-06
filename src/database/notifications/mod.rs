//! The `notification` resource: per-user in-app notifications.
//!
//! Created server-side when something happens to a document a person cares
//! about (currently: their document got reviewed or approved). Scoped to the
//! caller via the proxy-asserted `x-qims-user-email` header — everyone only
//! ever sees, reads and clears their own.

pub mod clear;
pub mod list;
pub mod mark_read;
pub mod stream;

use std::sync::OnceLock;

use axum::{
    Router,
    routing::{get as get_method, post},
};
use surrealdb::types::SurrealValue;
use tokio::sync::broadcast;

use crate::database::Database;

/// In-process fan-out that pokes live SSE streams when a notification is
/// created (the payload is the recipient email — clients refetch the list).
/// A process-wide static because notifications are created deep inside
/// document handlers; threading a channel through every router's state would
/// touch every handler for what is one global bus in a single-process app.
static BUS: OnceLock<broadcast::Sender<String>> = OnceLock::new();

pub fn bus() -> &'static broadcast::Sender<String> {
    // Capacity only buffers briefly between poke and read; slow/dead
    // subscribers just miss pokes (they resync on reconnect/fallback poll).
    BUS.get_or_init(|| broadcast::channel(64).0)
}

/// Ensure the `notification` table exists, indexed by recipient (every query
/// filters on it).
pub async fn init(db: &Database) -> Result<(), surrealdb::Error> {
    crate::db_timed!(
        "define notification table",
        db.query(
            "DEFINE TABLE IF NOT EXISTS notification SCHEMALESS; \
             DEFINE INDEX IF NOT EXISTS ix_notification_recipient \
             ON notification FIELDS recipient;",
        )
    )?
    .check()?;
    Ok(())
}

/// Routes for the notifications resource, mounted at the application root.
pub fn routes() -> Router<Database> {
    Router::new()
        .route(
            "/notifications",
            get_method(list::list).delete(clear::clear_all),
        )
        .route("/notifications/stream", get_method(stream::stream))
        .route("/notifications/read", post(mark_read::mark_read))
        .route("/notifications/{id}", axum::routing::delete(clear::clear_one))
}

#[derive(Debug, SurrealValue)]
struct NewNotification {
    recipient: String,
    /// What happened: `approved` or `reviewed`.
    kind: String,
    /// Record-id slug of the document the event happened on.
    document: String,
    document_number: String,
    title: String,
    /// Who did it (display name).
    actor: String,
    /// The review comment, when one was given.
    comment: String,
    read: bool,
    created_at: String,
}

/// Insert a notification. Best-effort: a failure is logged and swallowed —
/// notifying must never fail the action that triggered it.
#[allow(clippy::too_many_arguments)]
pub async fn notify(
    db: &Database,
    recipient: &str,
    kind: &str,
    document: &str,
    document_number: &str,
    title: &str,
    actor: &str,
    comment: &str,
) {
    let data = NewNotification {
        recipient: recipient.to_string(),
        kind: kind.to_string(),
        document: document.to_string(),
        document_number: document_number.to_string(),
        title: title.to_string(),
        actor: actor.to_string(),
        comment: comment.to_string(),
        read: false,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let result = crate::db_timed!(
        "create notification",
        db.query("CREATE notification CONTENT $data").bind(("data", data))
    );
    match result {
        Ok(_) => {
            log::info!("notified {recipient}: {kind} on {document_number}");
            // Poke any live streams for this person; no listeners is fine.
            let _ = bus().send(recipient.to_string());
        }
        Err(e) => log::error!("could not notify {recipient}: {e}"),
    }
}
