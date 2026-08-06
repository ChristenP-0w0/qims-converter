//! `PUT /documents/{id}` — edit a document.
//!
//! Behaviour depends on the current status:
//! - **Draft / Under Review**: edited in place. Status, author, approvals and
//!   reviews are preserved; `updated_at` is bumped. A `reason` (when supplied)
//!   is appended to the `changes` audit trail.
//! - **Approved**: the approved version is left untouched and *live*. The edit
//!   forks a brand-new **Draft** revision that points back at its parent via
//!   `revision_of`, so it re-enters the approval workflow. When that revision is
//!   later approved it supersedes (archives) the parent of the same number.

use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use surrealdb::types::{SurrealValue, Value};

use super::model::{Event, UpdateDocument};
use crate::database::Database;
use crate::error::AppError;

#[derive(Debug, Deserialize)]
pub struct UpdatePayload {
    #[serde(flatten)]
    doc: UpdateDocument,
    /// Why the change was made (recorded only when non-empty).
    #[serde(default)]
    reason: String,
    /// Which sections the writer changed, e.g. ["Content", "Edition"].
    #[serde(default)]
    changed_sections: Vec<String>,
    /// Who made the change (signed-in user placeholder).
    #[serde(default)]
    editor: String,
}

/// A single change/revision entry recorded on a document.
#[derive(Debug, SurrealValue)]
struct ChangeEntry {
    editor: String,
    reason: String,
    sections: Vec<String>,
    at: String,
}

/// Content for a forked revision created from an approved document.
#[derive(Debug, SurrealValue)]
struct NewRevision {
    title: String,
    kind: String,
    body: String,
    date: String,
    author: String,
    author_email: String,
    status: String,
    approved_by: Vec<String>,
    document_number: String,
    document_type: String,
    department: String,
    edition: u32,
    page_setup: String,
    /// Original imported file, carried over from the parent so the source
    /// stays downloadable on the new revision.
    source_file: String,
    source_name: String,
    source_mime: String,
    /// Slug of the approved document this revision was forked from.
    revision_of: String,
    created_at: String,
    updated_at: String,
    changes: Vec<ChangeEntry>,
    events: Vec<Event>,
}

pub async fn update(
    State(db): State<Database>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<UpdatePayload>,
) -> Result<Json<JsonValue>, AppError> {
    crate::database::users::require_writer(&db, &headers).await?;
    let mut doc = payload.doc;
    doc.validate()?;
    let now = chrono::Utc::now().to_rfc3339();
    doc.updated_at = now.clone();

    // Look up the current record to decide how the edit is applied.
    let mut sel = crate::db_timed!(
        "load document",
        db.query("SELECT * FROM type::record('document', $id)")
            .bind(("id", id.clone()))
    )?;
    let rows: Vec<Value> = sel.take(0)?;
    let current = match rows.into_iter().next() {
        Some(v) => v.into_json_value(),
        None => return Err(AppError::NotFound),
    };
    let status = current
        .get("status")
        .and_then(JsonValue::as_str)
        .unwrap_or("");
    let author = current
        .get("author")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .to_string();

    // Editing an approved document forks a new draft revision; the approved
    // version stays live until this revision is itself approved.
    if status == "Approved" {
        // The edition is SYSTEM-managed after creation: a revision is always
        // exactly base + 1, whatever the client sent.
        let base_edition = current
            .get("edition")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0) as u32;
        let next_edition = base_edition + 1;

        let actor = if payload.editor.trim().is_empty() {
            author.clone()
        } else {
            payload.editor.clone()
        };
        let changes = if payload.reason.trim().is_empty() {
            Vec::new()
        } else {
            vec![ChangeEntry {
                editor: actor.clone(),
                reason: payload.reason.clone(),
                sections: payload.changed_sections.clone(),
                at: now.clone(),
            }]
        };
        let events = vec![Event {
            kind: "created".to_string(),
            actor: actor.clone(),
            detail: payload.reason.clone(),
            sections: payload.changed_sections.clone(),
            at: now.clone(),
        }];
        let revision = NewRevision {
            title: doc.title,
            kind: doc.kind,
            body: doc.body,
            date: doc.date,
            author,
            author_email: current
                .get("author_email")
                .and_then(JsonValue::as_str)
                .unwrap_or("")
                .to_string(),
            status: "Draft".to_string(),
            approved_by: Vec::new(),
            document_number: doc.document_number,
            document_type: doc.document_type,
            department: doc.department,
            edition: next_edition,
            page_setup: doc.page_setup,
            source_file: current
                .get("source_file")
                .and_then(JsonValue::as_str)
                .unwrap_or("")
                .to_string(),
            source_name: current
                .get("source_name")
                .and_then(JsonValue::as_str)
                .unwrap_or("")
                .to_string(),
            source_mime: current
                .get("source_mime")
                .and_then(JsonValue::as_str)
                .unwrap_or("")
                .to_string(),
            revision_of: id.clone(),
            created_at: now.clone(),
            updated_at: now.clone(),
            changes,
            events,
        };

        log::info!(
            "forking approved document:{} into a new draft revision by {}",
            id,
            payload.editor
        );

        let mut res = crate::db_timed!(
            "fork revision",
            db.query("CREATE document CONTENT $data")
                .bind(("data", revision))
        )?;
        let created: Vec<Value> = res.take(0)?;
        let record = match created.into_iter().next() {
            Some(v) => v.into_json_value(),
            None => return Err(AppError::Internal("failed to create revision".into())),
        };
        return Ok(Json(record));
    }

    // Draft / Under Review: edit in place. The edition is system-managed —
    // it never changes on an in-place edit, whatever the client sent.
    doc.edition = current
        .get("edition")
        .and_then(JsonValue::as_u64)
        .unwrap_or(doc.edition as u64) as u32;
    log::info!("editing document:{} '{}'", id, doc.title);
    let actor = if payload.editor.trim().is_empty() {
        author.clone()
    } else {
        payload.editor.clone()
    };

    let mut res = crate::db_timed!(
        "update document",
        db.query("UPDATE type::record('document', $id) MERGE $data RETURN AFTER")
            .bind(("id", id.clone()))
            .bind(("data", doc))
    )?;
    let updated: Vec<Value> = res.take(0)?;
    let mut record = match updated.into_iter().next() {
        Some(v) => v,
        None => return Err(AppError::NotFound),
    };

    // Record the edit in the activity log (skip pure no-op saves).
    if !payload.changed_sections.is_empty() || !payload.reason.trim().is_empty() {
        let mut res_ev = crate::db_timed!(
            "log edit event",
            db.query(
                "UPDATE type::record('document', $id) SET events = array::append(events ?? [], { \
                 kind: 'edited', actor: $actor, detail: $reason, sections: $sections, at: $at \
                 }) RETURN AFTER",
            )
            .bind(("id", id.clone()))
            .bind(("actor", actor.clone()))
            .bind(("reason", payload.reason.clone()))
            .bind(("sections", payload.changed_sections.clone()))
            .bind(("at", now.clone()))
        )?;
        let after: Vec<Value> = res_ev.take(0)?;
        if let Some(v) = after.into_iter().next() {
            record = v;
        }
    }

    // Append a change-log entry (used by the revision diff) when a reason is given.
    if !payload.reason.trim().is_empty() {
        log::info!("change to document:{} by {} — {}", id, actor, payload.reason);
        let mut res2 = crate::db_timed!(
            "log change entry",
            db.query(
                "UPDATE type::record('document', $id) SET changes = array::append(changes ?? [], { \
                 editor: $editor, reason: $reason, sections: $sections, at: $at \
                 }) RETURN AFTER",
            )
            .bind(("id", id))
            .bind(("editor", actor))
            .bind(("reason", payload.reason))
            .bind(("sections", payload.changed_sections))
            .bind(("at", now))
        )?;
        let after: Vec<Value> = res2.take(0)?;
        if let Some(v) = after.into_iter().next() {
            record = v;
        }
    }

    Ok(Json(record.into_json_value()))
}
