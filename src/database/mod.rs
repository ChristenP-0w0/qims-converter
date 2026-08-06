//! Persistence layer: a standalone SurrealDB server (started by dev.sh /
//! serve.sh with `surreal start`) that the backend talks to over HTTP.
//! HTTP (not WebSocket) deliberately: the WS client keeps auth/namespace as
//! per-connection session state, and its silent reconnects dropped that state
//! ("Anonymous access not allowed" / hangs after idle). HTTP sends both with
//! every request — stateless and immune to that class of failure.
//! Running as a server — instead of embedding the store in-process — lets
//! tools like Surrealist browse the database while the site is up. Each
//! resource lives in its own submodule (e.g. [`documents`]).

pub mod documents;
pub mod notifications;
pub mod users;

use surrealdb::Surreal;
use surrealdb::engine::remote::http::{Client, Http};
use surrealdb::opt::auth::Root;

/// Shared handle to the SurrealDB connection, used as axum state.
pub type Database = Surreal<Client>;

/// Where the SurrealDB server listens (overridable via `QIMS_DB`).
const DEFAULT_ENDPOINT: &str = "127.0.0.1:8000";

/// Connect to the SurrealDB server and select the QIMS namespace/database.
/// Retries briefly so the backend can start in parallel with the server.
pub async fn connect() -> Result<Database, surrealdb::Error> {
    let endpoint =
        std::env::var("QIMS_DB").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string());
    let user = std::env::var("QIMS_DB_USER").unwrap_or_else(|_| "root".to_string());
    let pass = std::env::var("QIMS_DB_PASS").unwrap_or_else(|_| "root".to_string());

    log::info!("connecting to SurrealDB at http://{endpoint}");
    let mut attempt = 0;
    let db = loop {
        match Surreal::new::<Http>(endpoint.as_str()).await {
            Ok(db) => break db,
            Err(err) if attempt < 30 => {
                attempt += 1;
                if attempt == 1 {
                    log::info!("database not up yet, retrying…");
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let _ = err;
            }
            Err(err) => return Err(err),
        }
    };

    db.signin(Root {
        username: user,
        password: pass,
    })
    .await?;
    db.use_ns("qims").use_db("qims").await?;

    // Define tables up front so reads work on an empty database.
    documents::init(&db).await?;
    users::init(&db).await?;
    notifications::init(&db).await?;

    Ok(db)
}
