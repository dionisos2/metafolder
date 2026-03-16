use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rusqlite::Connection;
use uuid::Uuid;

use metafolder_core::entry::DatabaseId;

pub struct AppState {
    /// SQLite connection shared across axum handlers.
    /// Mutex because rusqlite is not async — we lock for the duration of each operation.
    pub conn: Arc<Mutex<Connection>>,
    /// Identifies this database instance. Regenerated on each restart (persistence deferred).
    pub db_id: DatabaseId,
}

impl AppState {
    pub fn new(root: &Path) -> anyhow::Result<Self> {
        let metafolder_dir = root.join(".metafolder");
        std::fs::create_dir_all(&metafolder_dir)
            .context("Failed to create the .metafolder directory")?;

        let db_path = metafolder_dir.join("db.sqlite");
        let conn = Connection::open(&db_path)
            .context("Failed to open the SQLite database")?;

        crate::db::init_schema(&conn)?;

        println!("Database loaded: {}", db_path.display());

        let db_id = Uuid::new_v4();

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            db_id,
        })
    }
}
