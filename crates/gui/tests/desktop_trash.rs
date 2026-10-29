//! The desktop's trash (doc "Trash"): a path no repository holds goes to the
//! trash of the freedesktop.org specification, where any file manager restores
//! it. A test binary of its own: one test points `XDG_DATA_HOME` somewhere
//! disposable, and the environment is the process's.

mod common;

use common::TempDir;

#[test]
fn a_path_outside_any_repository_goes_to_the_desktop_trash() {
    let data = TempDir::new("desktop_trash_data");
    let files = TempDir::new("desktop_trash_files");
    // SAFETY: the only test of this binary that reads or writes the environment.
    unsafe { std::env::set_var("XDG_DATA_HOME", data.path()) };

    let file = files.join("notes.txt");
    std::fs::write(&file, b"x").unwrap();
    let name = metafolder_gui::trash::trash_to_desktop(&file).unwrap();

    assert_eq!(name, "notes.txt");
    assert!(!file.exists(), "the file left its folder");
    let trash = data.join("Trash");
    assert_eq!(std::fs::read(trash.join("files/notes.txt")).unwrap(), b"x");
    let info = std::fs::read_to_string(trash.join("info/notes.txt.trashinfo")).unwrap();
    assert!(info.contains("Path=") && info.contains("DeletionDate="), "{info}");
}

#[test]
fn only_a_path_under_the_root_is_the_repository_s() {
    use metafolder_gui::trash::in_repository;
    use std::path::Path;
    let root = Path::new("/data/music");
    assert!(in_repository(root, Path::new("/data/music/a.mp3")));
    assert!(in_repository(root, Path::new("/data/music")));
    assert!(!in_repository(root, Path::new("/data/musicals/a.mp3")));
    assert!(!in_repository(root, Path::new("/home/me/a.txt")));
}
