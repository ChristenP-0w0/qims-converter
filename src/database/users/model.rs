//! The user data model: roles and the sync payload.

use serde::Deserialize;

/// What a signed-in person may do in QIMS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Editor rights plus user management.
    Admin,
    /// Writer rights plus reviewing/approving documents (never their own —
    /// document control requires an independent approver).
    Editor,
    /// May create, edit and submit documents — but not approve any.
    Writer,
    /// Read-only — the default for a first sign-in.
    Viewer,
}

/// Role strings as stored in the database / sent over the API.
pub const ALLOWED_ROLES: [&str; 4] = ["admin", "editor", "writer", "viewer"];

impl Role {
    /// Parse a stored role string; anything unknown degrades to read-only.
    pub fn parse(s: &str) -> Role {
        match s {
            "admin" => Role::Admin,
            "editor" => Role::Editor,
            "writer" => Role::Writer,
            _ => Role::Viewer,
        }
    }

    pub fn can_write(self) -> bool {
        matches!(self, Role::Admin | Role::Editor | Role::Writer)
    }

    /// Reviewing/approving documents is reserved for editors and admins.
    pub fn can_approve(self) -> bool {
        matches!(self, Role::Admin | Role::Editor)
    }
}

/// Payload the frontend sends after a session loads (`POST /users/sync`).
/// Profile fields only — the role is never client-controlled, and the email
/// is overridden by the proxy-asserted identity when present.
#[derive(Debug, Clone, Deserialize)]
pub struct SyncUser {
    #[serde(default)]
    pub email: String,
    /// Display name ("First Last", falling back to the email).
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub first_name: String,
    #[serde(default)]
    pub last_name: String,
    #[serde(default)]
    pub username: String,
    /// The account's id in the Main API (stringified — it may be numeric).
    #[serde(default)]
    pub main_api_id: String,
}
