//! What writes left pending is folded in idle time, one list per step (doc
//! "Folding in idle time"). The worker thread only calls `folding::step`; the
//! tests call it themselves, so nothing waits on time.

use std::sync::Arc;
use std::time::Duration;

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::log::Writer;
use metafolder_daemon::state::RepoState;
use metafolder_daemon::store::Rows;
use metafolder_daemon::{folding, repo};
use uuid::Uuid;

mod common;
use common::TempDir;

fn setup() -> (Arc<RepoState>, TempDir) {
    let root = TempDir::new("folding");
    common::seed_config(&root);
    let opened = repo::init_repository(&root, None, None, false).unwrap();
    (Arc::new(RepoState::from_opened_with(opened, &common::settings())), root)
}

fn pending(repo: &RepoState) -> usize {
    let conn = repo.conn.lock().unwrap();
    let kv = Rows::as_kv(&*conn).unwrap();
    kv.pending_mass().unwrap() + kv.pending_events("tag").unwrap()
}

#[test]
fn a_step_folds_one_list_once_the_repository_has_been_idle() {
    let (repo, _root) = setup();
    {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let uuids: Vec<Uuid> = (0..50)
            .map(|_| {
                w.create_metarecord(vec![Field::new("tag", Value::String("a".into()))])
                    .unwrap()
                    .uuid
            })
            .collect();
        w.mass_add("tag", Value::String("b".into()), &uuids).unwrap();
        w.commit().unwrap();
    }
    assert!(pending(&repo) > 0);

    // Activity has just been signalled: not idle for half a minute yet.
    repo.note_activity();
    assert_eq!(folding::step(&repo, Duration::from_secs(30)).unwrap(), None);
    assert!(pending(&repo) > 0, "nothing was folded");

    // Idle: one list per step, then nothing.
    let mut steps = 0;
    while folding::step(&repo, Duration::ZERO).unwrap() == Some(true) {
        steps += 1;
        assert!(steps < 10);
    }
    assert!(steps >= 1);
    assert_eq!(pending(&repo), 0);
    assert_eq!(folding::step(&repo, Duration::ZERO).unwrap(), Some(false));
}

#[test]
fn an_operation_in_flight_is_activity_and_the_quiet_starts_when_it_ends() {
    let (repo, _root) = setup();
    let hour = Duration::from_secs(3600);
    let long_ago = std::time::Instant::now() - hour;
    repo.activity.note_at(long_ago);
    assert!(repo.activity.idle_for() >= hour);
    assert_eq!(folding::step(&repo, Duration::from_secs(30)).unwrap(), Some(false));

    let operation = folding::begin_operation(&repo);
    // However long it lasts, it is not idle…
    repo.activity.note_at(long_ago);
    assert_eq!(repo.activity.idle_for(), Duration::ZERO);
    assert_eq!(folding::step(&repo, Duration::from_secs(30)).unwrap(), None);
    // …and the quiet starts when it ends, not when it began.
    drop(operation);
    assert!(repo.activity.idle_for() < Duration::from_secs(30));
    assert_eq!(folding::step(&repo, Duration::from_secs(30)).unwrap(), None);
}
