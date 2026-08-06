//! The `user` resource: everyone who has signed in to QIMS, with their role.
//!
//! Accounts live in the Main API (OneUser) — QIMS mirrors only the people who
//! have actually signed in here into its own `user` table so roles can be
//! assigned per person. Caller identity comes from the `x-qims-user-email`
//! request header, which the Next.js proxy sets from the *validated* session
//! cookie and never passes through from the browser. A request without that
//! header did not come through the proxy — that is local development or
//! `AUTH_DISABLED` (the backend listens on loopback only) and is fully
//! trusted.

pub mod list;
pub mod model;
pub mod set_role;
pub mod sync;

use axum::{
    Router,
    http::HeaderMap,
    routing::{get as get_method, post, put},
};
use surrealdb::types::Value;

use crate::database::Database;
use crate::error::AppError;
use model::Role;

/// The permanent administrator: created as an admin on first sign-in and
/// impossible to demote.
///
/// This placeholder is deliberately not a real address — set
/// `QIMS_ADMIN_EMAIL` to your own administrator on every deployment, or the
/// bootstrap admin is an account nobody can sign in as.
const BOOTSTRAP_ADMIN: &str = "admin@example.com";

pub fn bootstrap_admin() -> String {
    std::env::var("QIMS_ADMIN_EMAIL").unwrap_or_else(|_| BOOTSTRAP_ADMIN.to_string())
}

/// The signed-in caller's email, as asserted by the authenticating proxy.
pub fn caller_email(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-qims-user-email")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
}

/// The caller's role. No identity header means no proxy — trusted local
/// context (see module docs). The bootstrap admin is an admin even before
/// their `user` record exists; unknown emails are read-only.
pub async fn caller_role(db: &Database, headers: &HeaderMap) -> Result<Role, AppError> {
    let Some(email) = caller_email(headers) else {
        return Ok(Role::Admin);
    };
    if email == bootstrap_admin() {
        return Ok(Role::Admin);
    }
    let mut res = crate::db_timed!(
        "look up caller role",
        db.query("SELECT VALUE role FROM user WHERE email = $email LIMIT 1")
            .bind(("email", email))
    )?;
    let roles: Vec<Value> = res.take(0)?;
    Ok(roles
        .into_iter()
        .next()
        .map(|v| Role::parse(v.into_json_value().as_str().unwrap_or("")))
        .unwrap_or(Role::Viewer))
}

/// Guard: document writes require the writer (or admin) role.
pub async fn require_writer(db: &Database, headers: &HeaderMap) -> Result<(), AppError> {
    if caller_role(db, headers).await?.can_write() {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "your account has read-only access — ask an administrator for the writer role"
                .into(),
        ))
    }
}

/// Guard: reviewing/approving documents requires the editor (or admin) role.
pub async fn require_editor(db: &Database, headers: &HeaderMap) -> Result<(), AppError> {
    if caller_role(db, headers).await?.can_approve() {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "reviewing and approving documents requires the editor role".into(),
        ))
    }
}

/// Guard: user management requires the admin role.
pub async fn require_admin(db: &Database, headers: &HeaderMap) -> Result<(), AppError> {
    if caller_role(db, headers).await? == Role::Admin {
        Ok(())
    } else {
        Err(AppError::Forbidden("administrator access required".into()))
    }
}

/// Ensure the `user` table exists, with emails unique so concurrent first
/// sign-ins can never create duplicate people.
pub async fn init(db: &Database) -> Result<(), surrealdb::Error> {
    crate::db_timed!(
        "define user table",
        db.query(
            "DEFINE TABLE IF NOT EXISTS user SCHEMALESS; \
             DEFINE INDEX IF NOT EXISTS ux_user_email ON user FIELDS email UNIQUE;",
        )
    )?
    .check()?;
    Ok(())
}

/// Routes for the users resource, mounted at the application root.
pub fn routes() -> Router<Database> {
    Router::new()
        .route("/users", get_method(list::list))
        .route("/users/sync", post(sync::sync))
        .route("/users/{id}/role", put(set_role::set_role))
}
