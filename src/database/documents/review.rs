//! `POST /documents/{id}/review` — record an approval or a review, each with a
//! comment. Approving adds the reviewer to `approved_by`, promotes the document
//! to Approved, and archives the previous approved document of the same number.

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

#[derive(Debug, Deserialize)]
pub struct ReviewInput {
    /// Who is approving/reviewing (auto-filled from the signed-in user).
    pub reviewer: String,
    /// "approve" or "review".
    pub decision: String,
    /// Free-text review comment (optional).
    #[serde(default)]
    pub comment: String,
}

pub async fn review(
    State(db): State<Database>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<ReviewInput>,
) -> Result<Json<JsonValue>, AppError> {
    // Reviewing/approving is reserved for editors and admins — writers only
    // author and submit.
    crate::database::users::require_editor(&db, &headers).await?;
    if input.reviewer.trim().is_empty() {
        return Err(AppError::BadRequest("reviewer is required".into()));
    }
    let is_approval = match input.decision.as_str() {
        "approve" => true,
        "review" => false,
        other => return Err(AppError::BadRequest(format!("invalid decision: {other}"))),
    };
    let ev_kind = if is_approval { "approved" } else { "reviewed" };
    // Kept for the supersede/notify steps, since the inputs move into binds.
    let actor = input.reviewer.clone();
    let comment = input.comment.clone();

    // Load the authorship BEFORE recording anything: nobody approves their
    // own document, whatever their role — document control requires an
    // independent approver.
    let mut sel = crate::db_timed!(
        "load document authorship",
        db.query("SELECT author, author_email FROM type::record('document', $id)")
            .bind(("id", id.clone()))
    )?;
    let rows: Vec<Value> = sel.take(0)?;
    let authorship = match rows.into_iter().next() {
        Some(v) => v.into_json_value(),
        None => return Err(AppError::NotFound),
    };
    let get = |key: &str| {
        authorship
            .get(key)
            .and_then(JsonValue::as_str)
            .unwrap_or("")
            .to_string()
    };
    let mut author_email = get("author_email");
    if author_email.is_empty() {
        // Documents created before author emails were stored: map the
        // author's display name through the user table.
        let author = get("author");
        if !author.is_empty() {
            let mut res = crate::db_timed!(
                "look up author email",
                db.query("SELECT VALUE email FROM user WHERE name = $name LIMIT 1")
                    .bind(("name", author))
            )?;
            let emails: Vec<Value> = res.take(0)?;
            author_email = emails
                .into_iter()
                .next()
                .map(|v| v.into_json_value().as_str().unwrap_or("").to_string())
                .unwrap_or_default();
        }
    }
    let caller = crate::database::users::caller_email(&headers);
    if is_approval
        && !author_email.is_empty()
        && caller.as_deref() == Some(author_email.as_str())
    {
        return Err(AppError::Forbidden(
            "you cannot approve your own document — another editor must approve it"
                .into(),
        ));
    }

    log::info!("{} on document:{} by {}", input.decision, id, input.reviewer);

    let now = chrono::Utc::now().to_rfc3339();

    // Approving promotes the document and records the reviewer in
    // approved_by (deduplicated). A comment-review never touches the status:
    // an Approved document stays Approved and just collects the comment.
    let status_clause = if is_approval {
        "status = 'Approved', "
    } else {
        ""
    };
    let approver_clause = if is_approval {
        "approved_by = array::distinct(array::append(approved_by ?? [], $reviewer)),"
    } else {
        ""
    };
    let sql = format!(
        "UPDATE type::record('document', $id) SET \
         {status_clause}updated_at = $updated, \
         {approver_clause} \
         reviews = array::append(reviews ?? [], {{ \
             reviewer: $reviewer, decision: $decision, comment: $comment, at: $updated \
         }}), \
         events = array::append(events ?? [], {{ \
             kind: $ev_kind, actor: $reviewer, detail: $comment, sections: [], at: $updated \
         }}) \
         RETURN AFTER"
    );

    let mut res = crate::db_timed!(
        "record review",
        db.query(sql.as_str())
            .bind(("id", id.clone()))
            .bind(("updated", now.clone()))
            .bind(("reviewer", input.reviewer))
            .bind(("decision", input.decision))
            .bind(("comment", input.comment))
            .bind(("ev_kind", ev_kind))
    )?;
    let updated: Vec<Value> = res.take(0)?;
    let doc = match updated.into_iter().next() {
        Some(v) => v.into_json_value(),
        None => return Err(AppError::NotFound),
    };

    // On approval, supersede the previous approved edition of the same number.
    if is_approval {
        if let Some(number) = doc.get("document_number").and_then(|v| v.as_str()) {
            crate::db_timed!(
                "supersede previous editions",
                db.query(
                    "UPDATE document SET status = 'Archived', updated_at = $updated, \
                     events = array::append(events ?? [], { \
                       kind: 'archived', actor: $actor, \
                       detail: 'Superseded by a newer approved edition', sections: [], at: $updated \
                     }) \
                     WHERE document_number = $num AND status = 'Approved' \
                     AND id != type::record('document', $id)",
                )
                .bind(("num", number.to_string()))
                .bind(("updated", now))
                .bind(("id", id.clone()))
                .bind(("actor", actor.clone()))
            )?;
            log::info!("archived previous approved editions of {number}");
        }
    }

    // Notify the document's author that their submission got attention —
    // unless they are the one acting on it. Authorship was resolved above.
    let field = |key: &str| {
        doc.get(key)
            .and_then(JsonValue::as_str)
            .unwrap_or("")
            .to_string()
    };
    if !author_email.is_empty() && caller.as_deref() != Some(author_email.as_str()) {
        crate::database::notifications::notify(
            &db,
            &author_email,
            ev_kind,
            &id,
            &field("document_number"),
            &field("title"),
            &actor,
            &comment,
        )
        .await;
    }

    Ok(Json(doc))
}
