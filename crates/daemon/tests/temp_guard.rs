//! The test harness's own safety net (`tests/common`): a disposable directory
//! removes itself even when the test that owns it panics.
//!
//! This is the case that mattered. Every suite used to end with an explicit
//! `remove_dir_all`, which is exactly the line a panic skips — so a failing run
//! left its repository behind, and enough runs filled the disk. A full disk is
//! not a quiet failure: SQLite answers "database or disk is full" and the
//! watcher's flush fails on it.

mod common;
use common::{tests_root, TempDir, TempFile};

#[test]
fn test_a_temp_dir_removes_itself_when_its_test_panics() {
    let path = {
        let dir = TempDir::new("guard_panic");
        let path = dir.path().to_path_buf();
        assert!(path.is_dir(), "the directory exists while the guard is alive");

        // A test body that fails halfway, with the guard owned by the frame
        // being unwound — `remove_dir_all` at the end of it would never run.
        let dir = std::panic::AssertUnwindSafe(dir);
        let outcome = std::panic::catch_unwind(move || {
            let _dir = dir;
            panic!("the test failed");
        });
        assert!(outcome.is_err(), "the panic was caught, not swallowed");
        path
    };
    assert!(!path.exists(), "the directory must be gone with the guard");
}

#[test]
fn test_temp_paths_live_under_one_purgeable_parent() {
    // Whatever a crashed run leaves behind is one `rm -rf` away.
    let dir = TempDir::new("guard_parent");
    let file = TempFile::new("guard_parent_file", b"x");
    assert!(dir.starts_with(tests_root()), "{:?} is outside {:?}", dir.path(), tests_root());
    assert!(file.starts_with(tests_root()), "{file:?} is outside {:?}", tests_root());
}

#[test]
fn test_a_temp_file_takes_its_directory_with_it() {
    let dir = {
        let file = TempFile::new("guard_file", b"content");
        assert_eq!(std::fs::read(&*file).unwrap(), b"content");
        file.parent().expect("a file in a directory").to_path_buf()
    };
    assert!(!dir.exists(), "the file's directory goes with it");
}
