//! Direction-awareness of the `skip` restoration in coordinated navigation
//! (spec-event-log "skip"; review #6). A skipped `file_moved` must rewind
//! `mfr_path` to the location the file is *recorded at before the step* — the
//! snapshot the step did not apply: `is_new=1` on an inverse, `is_new=0` on a
//! forward (redo) step.

use std::sync::Arc;

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::db;
use metafolder_daemon::log::{self, OpType, Writer};
use metafolder_daemon::repo;
use metafolder_daemon::state::RepoState;
use uuid::Uuid;

/// Builds a repo with a file record moved from `/a.txt` to `/b.txt`, returning
/// (repo, root_uuid, record_uuid, create_op_id, move_op_id).
fn setup_move(prefix: &str) -> (Arc<RepoState>, Uuid, i64, i64) {
    let root = std::env::temp_dir().join(format!("metafolder_cskip_{prefix}_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let opened = repo::init_repository(&root, None).unwrap();
    let repo = Arc::new(RepoState::from_opened(opened));
    let db_id = repo.config.repo_uuid;

    let root_uuid = {
        let conn = repo.conn.lock().unwrap();
        db::find_tree_child(&conn, "mfr_path", None, "").unwrap().unwrap()
    };

    // create the record at /a.txt
    let record = {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
        let m = w
            .create_metarecord(vec![Field::new(
                "mfr_path",
                Value::TreeRef { parent: Some(root_uuid), name: "a.txt".into() },
            )])
            .unwrap();
        w.commit().unwrap();
        m.uuid
    };
    let create_op = repo.conn.lock().unwrap();
    let create_op_id = log::get_head(&create_op).unwrap().unwrap();
    drop(create_op);

    // move it to /b.txt (a file_moved op: before=/a.txt is_new=0, after=/b.txt is_new=1)
    {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
        w.set_field_as(
            OpType::FileMoved,
            record,
            "mfr_path",
            Value::TreeRef { parent: Some(root_uuid), name: "b.txt".into() },
        )
        .unwrap();
        w.commit().unwrap();
    }
    let move_op_id = log::get_head(&repo.conn.lock().unwrap()).unwrap().unwrap();

    (repo, db_id, create_op_id, move_op_id)
}

/// The single enqueued `restore_set_path` name component (the rewind target).
fn restoration_name(repo: &RepoState) -> String {
    let conn = repo.conn.lock().unwrap();
    conn.query_row(
        "SELECT to_path FROM pending_operation WHERE op_type = 'restore_set_path'",
        [],
        |r| r.get::<_, String>(0),
    )
    .unwrap()
}

#[test]
fn forward_skip_rewinds_to_the_pre_move_location() {
    let (repo, db_id, create_op, move_op) = setup_move("forward");

    // Roll back the move, then redo it forward with skip.
    {
        let mut conn = repo.conn.lock().unwrap();
        log::navigate(&mut conn, db_id, Some(create_op)).unwrap();
        log::coordinated_step(&mut conn, db_id, Some(move_op), true).unwrap();
    }

    // The forward step applied the post-move location (/b.txt); the rewind must
    // target the pre-move one (/a.txt), not the location the move would set.
    assert_eq!(restoration_name(&repo), "a.txt");

    std::fs::remove_dir_all(&repo.config.root).ok();
}

#[test]
fn inverse_skip_rewinds_to_the_current_location() {
    let (repo, db_id, create_op, move_op) = setup_move("inverse");

    // HEAD is at the move; roll it back one step with skip.
    {
        let mut conn = repo.conn.lock().unwrap();
        log::coordinated_step(&mut conn, db_id, Some(create_op), true).unwrap();
    }
    let _ = move_op;

    // The inverse step applied the pre-move location (/a.txt); the rewind keeps
    // the file at its current recorded location (/b.txt).
    assert_eq!(restoration_name(&repo), "b.txt");

    std::fs::remove_dir_all(&repo.config.root).ok();
}
