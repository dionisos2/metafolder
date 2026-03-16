use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use notify::Watcher;
use rusqlite::Connection;

use metafolder_core::entry::{DatabaseId, Field, Value};

pub fn start(root: PathBuf, conn: Arc<Mutex<Connection>>, db_id: DatabaseId) {
    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();

    let mut watcher = notify::RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        notify::Config::default(),
    )
    .expect("Failed to create watcher");

    watcher
        .watch(&root, notify::RecursiveMode::Recursive)
        .expect("Failed to watch root directory");

    tokio::task::spawn_blocking(move || {
        let _watcher = watcher; // keep alive
        for res in rx {
            match res {
                Ok(event) => handle_event(event, &conn, db_id, &root),
                Err(e) => eprintln!("[watcher] error: {e}"),
            }
        }
    });
}

fn handle_event(
    event: notify::Event,
    conn: &Arc<Mutex<Connection>>,
    db_id: DatabaseId,
    root: &Path,
) {
    for path in &event.paths {
        if is_metafolder_path(path, root) {
            continue;
        }
        match event.kind {
            notify::EventKind::Create(_) => on_file_created(path, conn, db_id),
            notify::EventKind::Remove(_) => on_file_deleted(path, conn),
            _ => {}
        }
    }
}

fn is_metafolder_path(path: &Path, root: &Path) -> bool {
    path.starts_with(root.join(".metafolder"))
}

fn on_file_created(path: &Path, conn: &Arc<Mutex<Connection>>, db_id: DatabaseId) {
    let fields = vec![Field {
        name: "path".to_string(),
        value: Value::String(path.to_string_lossy().into_owned()),
    }];
    let conn = conn.lock().unwrap();
    if let Err(e) = crate::db::create_entry(&conn, db_id, fields) {
        eprintln!("[watcher] create_entry failed for {:?}: {e}", path);
    }
}

fn on_file_deleted(path: &Path, conn: &Arc<Mutex<Connection>>) {
    let path_str = path.to_string_lossy().into_owned();
    let conn = conn.lock().unwrap();
    match crate::db::find_entry_by_path(&conn, &path_str) {
        Ok(Some(uuid)) => {
            if let Err(e) = crate::db::clear_path(&conn, uuid) {
                eprintln!("[watcher] clear_path failed for {uuid}: {e}");
            }
        }
        Ok(None) => {} // not tracked, ignore
        Err(e) => eprintln!("[watcher] find_entry_by_path failed for {:?}: {e}", path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::{Event, EventKind, event::{CreateKind, RemoveKind, ModifyKind, DataChange}};
    use rusqlite::Connection;
    use uuid::Uuid;

    fn test_db() -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();
        Arc::new(Mutex::new(conn))
    }

    #[test]
    fn test_is_metafolder_path_db_file() {
        let root = Path::new("/repo");
        assert!(is_metafolder_path(Path::new("/repo/.metafolder/db.sqlite"), root));
    }

    #[test]
    fn test_is_metafolder_path_regular_file() {
        let root = Path::new("/repo");
        assert!(!is_metafolder_path(Path::new("/repo/music/a.mp3"), root));
    }

    #[test]
    fn test_is_metafolder_path_similar_name() {
        // starts_with matches full segments, so .metafolder_backup is not .metafolder
        let root = Path::new("/repo");
        assert!(!is_metafolder_path(Path::new("/repo/.metafolder_backup/x"), root));
    }

    #[test]
    fn test_handle_create_event() {
        let conn = test_db();
        let db_id = Uuid::new_v4();
        let root = PathBuf::from("/tmp/root");
        let event = Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![PathBuf::from("/tmp/root/song.mp3")],
            attrs: Default::default(),
        };

        handle_event(event, &conn, db_id, &root);

        let conn_guard = conn.lock().unwrap();
        let result = crate::db::find_entry_by_path(&conn_guard, "/tmp/root/song.mp3").unwrap();
        assert!(result.is_some(), "entry should have been created");
    }

    #[test]
    fn test_handle_remove_event() {
        let conn = test_db();
        let db_id = Uuid::new_v4();
        let root = PathBuf::from("/tmp/root");

        // First create an entry
        let create_event = Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![PathBuf::from("/tmp/root/song.mp3")],
            attrs: Default::default(),
        };
        handle_event(create_event, &conn, db_id, &root);

        let uuid = conn.lock().unwrap()
            .query_row("SELECT uuid FROM metadata LIMIT 1", [], |r| r.get::<_, Vec<u8>>(0))
            .map(|b| uuid::Uuid::from_bytes(b.try_into().unwrap()))
            .unwrap();

        // Then remove it
        let remove_event = Event {
            kind: EventKind::Remove(RemoveKind::File),
            paths: vec![PathBuf::from("/tmp/root/song.mp3")],
            attrs: Default::default(),
        };
        handle_event(remove_event, &conn, db_id, &root);

        let conn_guard = conn.lock().unwrap();
        // path should no longer be findable (it's Nothing now)
        let result = crate::db::find_entry_by_path(&conn_guard, "/tmp/root/song.mp3").unwrap();
        assert_eq!(result, None, "path should no longer resolve to an entry");
        // but the entry itself should still exist with path = Nothing
        let entry = crate::db::get_entry(&conn_guard, uuid).unwrap();
        let path_field = entry.fields.iter().find(|f| f.name == "path").unwrap();
        assert_eq!(path_field.value, Value::Nothing, "path should be Nothing, not deleted");
    }

    #[test]
    fn test_handle_metafolder_event_ignored() {
        let conn = test_db();
        let db_id = Uuid::new_v4();
        let root = PathBuf::from("/tmp/root");
        let event = Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![PathBuf::from("/tmp/root/.metafolder/db.sqlite")],
            attrs: Default::default(),
        };

        handle_event(event, &conn, db_id, &root);

        let conn_guard = conn.lock().unwrap();
        let result = crate::db::find_entry_by_path(
            &conn_guard, "/tmp/root/.metafolder/db.sqlite"
        ).unwrap();
        assert_eq!(result, None, ".metafolder paths should be ignored");
    }

    #[test]
    fn test_handle_modify_event_ignored() {
        let conn = test_db();
        let db_id = Uuid::new_v4();
        let root = PathBuf::from("/tmp/root");
        let event = Event {
            kind: EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            paths: vec![PathBuf::from("/tmp/root/song.mp3")],
            attrs: Default::default(),
        };

        handle_event(event, &conn, db_id, &root);

        let conn_guard = conn.lock().unwrap();
        let result = crate::db::find_entry_by_path(&conn_guard, "/tmp/root/song.mp3").unwrap();
        assert_eq!(result, None, "Modify events should not create entries");
    }
}
