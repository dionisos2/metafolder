use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use notify::Watcher;
use rusqlite::Connection;

use metafolder_core::entry::{DatabaseId, Field, Value};

pub struct WatcherHandle {
    // Keeps the watcher alive: dropping it closes the event channel,
    // which causes the background thread's loop to exit naturally.
    _watcher: notify::RecommendedWatcher,
    _thread: std::thread::JoinHandle<()>,
}

pub fn start(root: PathBuf, conn: Arc<Mutex<Connection>>, db_id: DatabaseId) -> WatcherHandle {
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

    let thread = std::thread::spawn(move || {
        for res in rx {
            match res {
                Ok(event) => handle_event(event, &conn, db_id, &root),
                Err(e) => eprintln!("[watcher] error: {e}"),
            }
        }
    });

    WatcherHandle { _watcher: watcher, _thread: thread }
}

fn handle_event(
    event: notify::Event,
    conn: &Arc<Mutex<Connection>>,
    db_id: DatabaseId,
    root: &Path,
) {
    use notify::event::{ModifyKind, RenameMode};

    match event.kind {
        notify::EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            if let [old, new] = event.paths.as_slice() {
                if !is_metafolder_path(old, root) {
                    on_file_renamed(old, new, conn, db_id);
                }
            }
        }
        kind => {
            for path in &event.paths {
                if is_metafolder_path(path, root) {
                    continue;
                }
                match kind {
                    notify::EventKind::Create(_) => on_file_created(path, conn, db_id),
                    notify::EventKind::Remove(_) => on_file_deleted(path, conn),
                    notify::EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                        on_file_deleted(path, conn)
                    }
                    notify::EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                        on_file_created(path, conn, db_id)
                    }
                    _ => {}
                }
            }
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
        Ok(None) => {}
        Err(e) => eprintln!("[watcher] find_entry_by_path failed for {:?}: {e}", path),
    }
}

fn on_file_renamed(old: &Path, new: &Path, conn: &Arc<Mutex<Connection>>, db_id: DatabaseId) {
    let old_str = old.to_string_lossy().into_owned();
    let new_str = new.to_string_lossy().into_owned();
    let conn = conn.lock().unwrap();

    // First try an exact path match (file rename).
    match crate::db::update_path(&conn, &old_str, &new_str) {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => { eprintln!("[watcher] update_path failed for {:?} → {:?}: {e}", old, new); return; }
    }

    // No exact match — try a prefix update (directory rename).
    match crate::db::update_path_prefix(&conn, &old_str, &new_str) {
        Ok(n) if n > 0 => return,
        Ok(_) => {
            // Unknown source: create a new entry (file moved in from outside the root).
            let fields = vec![Field {
                name: "path".to_string(),
                value: Value::String(new_str),
            }];
            if let Err(e) = crate::db::create_entry(&conn, db_id, fields) {
                eprintln!("[watcher] create_entry failed for {:?}: {e}", new);
            }
        }
        Err(e) => eprintln!("[watcher] update_path_prefix failed for {:?} → {:?}: {e}", old, new),
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
        crate::db::init_db(&conn).unwrap();
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

        let remove_event = Event {
            kind: EventKind::Remove(RemoveKind::File),
            paths: vec![PathBuf::from("/tmp/root/song.mp3")],
            attrs: Default::default(),
        };
        handle_event(remove_event, &conn, db_id, &root);

        let conn_guard = conn.lock().unwrap();
        let result = crate::db::find_entry_by_path(&conn_guard, "/tmp/root/song.mp3").unwrap();
        assert_eq!(result, None, "path should no longer resolve to an entry");
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
        assert_eq!(result, None, "Modify(Data) events should not create entries");
    }

    #[test]
    fn test_handle_rename_both_updates_path() {
        let conn = test_db();
        let db_id = Uuid::new_v4();
        let root = PathBuf::from("/tmp/root");

        let create_event = Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![PathBuf::from("/tmp/root/old.mp3")],
            attrs: Default::default(),
        };
        handle_event(create_event, &conn, db_id, &root);

        let rename_event = Event {
            kind: EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::Both)),
            paths: vec![PathBuf::from("/tmp/root/old.mp3"), PathBuf::from("/tmp/root/new.mp3")],
            attrs: Default::default(),
        };
        handle_event(rename_event, &conn, db_id, &root);

        let conn_guard = conn.lock().unwrap();
        assert!(crate::db::find_entry_by_path(&conn_guard, "/tmp/root/old.mp3").unwrap().is_none());
        assert!(crate::db::find_entry_by_path(&conn_guard, "/tmp/root/new.mp3").unwrap().is_some());
    }

    #[test]
    fn test_handle_rename_from_sets_path_to_nothing() {
        let conn = test_db();
        let db_id = Uuid::new_v4();
        let root = PathBuf::from("/tmp/root");

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

        let from_event = Event {
            kind: EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::From)),
            paths: vec![PathBuf::from("/tmp/root/song.mp3")],
            attrs: Default::default(),
        };
        handle_event(from_event, &conn, db_id, &root);

        let conn_guard = conn.lock().unwrap();
        let entry = crate::db::get_entry(&conn_guard, uuid).unwrap();
        let path_field = entry.fields.iter().find(|f| f.name == "path").unwrap();
        assert_eq!(path_field.value, Value::Nothing);
    }

    #[test]
    fn test_handle_directory_rename_updates_child_paths() {
        // When a directory is renamed, all entries whose paths are inside it
        // should have their paths updated to reflect the new directory name.
        let conn = test_db();
        let db_id = Uuid::new_v4();
        let root = PathBuf::from("/tmp/root");

        // Create entries for files inside the directory
        for name in &["file_a.mp3", "file_b.mp3"] {
            let event = Event {
                kind: EventKind::Create(CreateKind::File),
                paths: vec![PathBuf::from(format!("/tmp/root/subdir/{name}"))],
                attrs: Default::default(),
            };
            handle_event(event, &conn, db_id, &root);
        }

        // Rename the directory
        let rename_event = Event {
            kind: EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::Both)),
            paths: vec![
                PathBuf::from("/tmp/root/subdir"),
                PathBuf::from("/tmp/root/subdir_moved"),
            ],
            attrs: Default::default(),
        };
        handle_event(rename_event, &conn, db_id, &root);

        let conn_guard = conn.lock().unwrap();
        // Old paths must no longer exist
        assert!(crate::db::find_entry_by_path(&conn_guard, "/tmp/root/subdir/file_a.mp3").unwrap().is_none());
        assert!(crate::db::find_entry_by_path(&conn_guard, "/tmp/root/subdir/file_b.mp3").unwrap().is_none());
        // New paths must exist
        assert!(crate::db::find_entry_by_path(&conn_guard, "/tmp/root/subdir_moved/file_a.mp3").unwrap().is_some());
        assert!(crate::db::find_entry_by_path(&conn_guard, "/tmp/root/subdir_moved/file_b.mp3").unwrap().is_some());
    }

    #[test]
    fn test_handle_rename_to_creates_entry() {
        let conn = test_db();
        let db_id = Uuid::new_v4();
        let root = PathBuf::from("/tmp/root");

        let to_event = Event {
            kind: EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::To)),
            paths: vec![PathBuf::from("/tmp/root/arrived.mp3")],
            attrs: Default::default(),
        };
        handle_event(to_event, &conn, db_id, &root);

        let conn_guard = conn.lock().unwrap();
        assert!(crate::db::find_entry_by_path(&conn_guard, "/tmp/root/arrived.mp3").unwrap().is_some());
    }
}
