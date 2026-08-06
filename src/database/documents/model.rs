//! The document data model and its validation rules.

use serde::Deserialize;
use surrealdb::types::SurrealValue;

use crate::error::AppError;

/// Allowed document types (the kind of document).
pub const ALLOWED_TYPE: [&str; 4] = ["Policy", "Method", "Form", "Work Instruction"];
/// Allowed departments (the discipline that owns the document).
pub const ALLOWED_DEPARTMENT: [&str; 3] = ["Quality", "Chemistry", "Microbiology"];
/// Editor/content kind: a rich-text document or a spreadsheet.
pub const ALLOWED_KIND: [&str; 2] = ["document", "spreadsheet"];

/// A single entry in a document's activity log (its "commit history").
///
/// `kind` is one of: `created`, `edited`, `submitted`, `reviewed`, `approved`,
/// `archived`. `detail` carries the reason (edits) or comment (reviews);
/// `sections` lists what an edit touched.
#[derive(Debug, Clone, Deserialize, SurrealValue)]
pub struct Event {
    pub kind: String,
    pub actor: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub sections: Vec<String>,
    pub at: String,
}

/// Validate the enumerated fields shared by create and update.
fn validate_enums(kind: &str, document_type: &str, department: &str) -> Result<(), AppError> {
    if !ALLOWED_KIND.contains(&kind) {
        return Err(AppError::BadRequest(format!("invalid kind: {kind}")));
    }
    if !ALLOWED_TYPE.contains(&document_type) {
        return Err(AppError::BadRequest(format!(
            "invalid document_type: {document_type}"
        )));
    }
    if !ALLOWED_DEPARTMENT.contains(&department) {
        return Err(AppError::BadRequest(format!(
            "invalid department: {department}"
        )));
    }
    Ok(())
}

/// Payload the frontend sends when creating a document.
///
/// `status`, `approved_by` and the timestamps are server-controlled (defaulted
/// here, then set in the handler) — a new document is always a Draft.
#[derive(Debug, Clone, Deserialize, SurrealValue)]
pub struct CreateDocument {
    pub title: String,
    /// One of [`ALLOWED_KIND`] — determines how `body` is interpreted.
    pub kind: String,
    /// For a `document`: Tiptap HTML. For a `spreadsheet`: JSON-encoded grid.
    pub body: String,
    /// Document date, ISO-8601 (`YYYY-MM-DD`) from the form.
    pub date: String,
    /// Auto-filled from the signed-in user (placeholder until accounts exist).
    pub author: String,
    /// The author's account email — server-set from the session identity,
    /// used to address notifications about this document.
    #[serde(default)]
    pub author_email: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub approved_by: Vec<String>,
    pub document_number: String,
    /// One of [`ALLOWED_TYPE`] — Policy / Method / Form / Work Instruction.
    pub document_type: String,
    /// One of [`ALLOWED_DEPARTMENT`] — Quality / Chemistry / Microbiology.
    pub department: String,
    /// Current edition (version) number, e.g. 12 for "ED12".
    pub edition: u32,
    /// JSON-encoded page geometry for imported documents ("" = defaults).
    #[serde(default)]
    pub page_setup: String,
    /// Original imported file: stored name under `data/originals` ("" = not
    /// imported). Set from the /convert response, served by
    /// `GET /documents/{id}/original`.
    #[serde(default)]
    pub source_file: String,
    /// The original file's name as the user uploaded it.
    #[serde(default)]
    pub source_name: String,
    /// The original file's MIME type (e.g. "application/pdf").
    #[serde(default)]
    pub source_mime: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    /// Activity log — server-controlled; seeded with a `created` event.
    #[serde(default)]
    pub events: Vec<Event>,
}

impl CreateDocument {
    pub fn validate(&self) -> Result<(), AppError> {
        if self.title.trim().is_empty() {
            return Err(AppError::BadRequest("title is required".into()));
        }
        if self.author.trim().is_empty() {
            return Err(AppError::BadRequest("author is required".into()));
        }
        if self.edition < 1 {
            return Err(AppError::BadRequest("edition must be at least 1".into()));
        }
        validate_enums(&self.kind, &self.document_type, &self.department)
    }
}

/// Payload for editing an existing (Draft) document. Only content/metadata
/// fields are editable — `status`, `author`, `approved_by`, `reviews` and
/// `created_at` are preserved by using a `MERGE`.
#[derive(Debug, Clone, Deserialize, SurrealValue)]
pub struct UpdateDocument {
    pub title: String,
    pub kind: String,
    pub body: String,
    pub date: String,
    pub document_number: String,
    pub document_type: String,
    pub department: String,
    pub edition: u32,
    /// JSON-encoded page geometry for imported documents ("" = defaults).
    #[serde(default)]
    pub page_setup: String,
    #[serde(default)]
    pub updated_at: String,
}

impl UpdateDocument {
    pub fn validate(&self) -> Result<(), AppError> {
        if self.title.trim().is_empty() {
            return Err(AppError::BadRequest("title is required".into()));
        }
        if self.edition < 1 {
            return Err(AppError::BadRequest("edition must be at least 1".into()));
        }
        validate_enums(&self.kind, &self.document_type, &self.department)
    }
}
