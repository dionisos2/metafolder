//! The mount table, end to end: drives plugged in and out of a watched
//! repository while work goes on, held to the oracle of `early_journey.rs` —
//! after each step a settled watcher and a full reconcile agree (doc "Testing
//! the watcher") — and to the freeze of an unplugged drive (doc "Offline
//! subtrees").
//!
//! Real mounts, so each test runs again in a user namespace of its own
//! (`common::in_userns`): the daemon asks the kernel's mount table whether a
//! directory is a mount point, and the simulated kernel keeps its own table
//! in step with the real one. Its own binary: the namespace is the whole
//! process's.

use std::future::Future;
use std::time::Duration;

use axum::Router;

mod common;
use common::journey::{
    journey_repo, orphan_count, reconcile, reconcile_agrees, request, settle_on, tracked_paths,
};
use common::{Fs, Regime, TempDir};

/// Runs `scenario` under `regime` inside a user namespace.
fn run<F: Future<Output = ()>>(test: &str, regime: Regime, scenario: fn(Regime) -> F) {
    if !common::in_userns(test) {
        return;
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(scenario(regime));
}

macro_rules! on_both_sources {
    ($($name:ident),* $(,)?) => {
        mod inotify {
            $(
                #[test]
                fn $name() {
                    let test = concat!("inotify::", stringify!($name));
                    super::run(test, super::Regime::Inotify, super::$name)
                }
            )*
        }
        mod simulated {
            $(
                #[test]
                fn $name() {
                    let test = concat!("simulated::", stringify!($name));
                    super::run(test, super::Regime::Simulated, super::$name)
                }
            )*
        }
    };
}

/// The daemon reads the mount table at most once a second (`mount::table`):
/// what it says right after a mount change can be a second old.
async fn mount_table_read_again() {
    tokio::time::sleep(Duration::from_millis(1100)).await;
}

/// A drive kept mounted outside the repository, to be bound into it and taken
/// out again with its content intact — a drive unplugged and plugged back.
fn drive(fs: &Fs, name: &str) -> TempDir {
    let store = TempDir::new(&format!("drive_{name}"));
    fs.mount_tmpfs(&store).unwrap();
    store
}

/// The declared mount points, `(path, state)`, by path.
async fn mounts(app: &Router, repo: &str) -> Vec<(String, String)> {
    let (_, body) = request(app, "GET", &format!("/repos/{repo}/mounts"), None).await;
    let list = body["mounts"].as_array().unwrap_or_else(|| panic!("mounts: {body}"));
    list.iter()
        .map(|m| (m["path"].as_str().unwrap_or("?").to_string(), m["state"].to_string()))
        .collect()
}

/// What the orphan scan reports, by path — never anything under an unplugged
/// drive.
async fn orphans_scanned(app: &Router, repo: &str) -> Vec<String> {
    let (_, body) = request(app, "POST", &format!("/repos/{repo}/orphans/scan"), None).await;
    let mut out: Vec<String> = body["orphans"]
        .as_array()
        .unwrap_or_else(|| panic!("orphan scan: {body}"))
        .iter()
        .map(|o| o["stale_path"].as_str().map_or_else(|| o.to_string(), str::to_string))
        .collect();
    out.sort();
    out
}

fn sorted(paths: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = paths.iter().map(|s| s.to_string()).collect();
    out.sort();
    out
}

// ── A drive plugged in and out with work going on ─────────────────────────────

async fn a_drive_plugged_in_and_out_with_work_going_on(regime: Regime) {
    let fs = regime.fs();
    let (app, repo, root) = journey_repo("drive", regime).await;
    fs.create_dir(root.join("docs")).unwrap();
    fs.write(root.join("docs/a.txt"), b"a").unwrap();
    fs.create_dir(root.join("usb")).unwrap();
    settle_on(&app, &repo, &["", "docs", "docs/a.txt", "usb"]).await;

    // A drive holding photos already, plugged in at `usb`.
    let store = drive(&fs, "photos");
    std::fs::create_dir(store.join("album")).unwrap();
    std::fs::write(store.join("album/b.jpg"), b"b").unwrap();
    std::fs::write(store.join("photo.jpg"), b"p").unwrap();
    fs.bind_mount(&store, root.join("usb")).unwrap();
    mount_table_read_again().await;

    // Plugging a drive in starts nothing (doc "Offline subtrees"): what it
    // holds is found by a reconcile, which records the mount point.
    let result = reconcile(&app, &repo, "the drive plugged in").await;
    assert_eq!(result["created"], 3, "{result}");
    let mut on_drive = vec!["", "docs", "docs/a.txt", "usb", "usb/album", "usb/album/b.jpg"];
    on_drive.push("usb/photo.jpg");
    assert_eq!(tracked_paths(&app, &repo).await, sorted(&on_drive));
    assert_eq!(mounts(&app, &repo).await, [("/usb".to_string(), "\"online\"".to_string())]);
    reconcile_agrees(&app, &repo, "the drive plugged in").await;

    // Work on the drive and beside it.
    fs.write(root.join("usb/new.jpg"), b"n").unwrap();
    fs.rename(root.join("usb/photo.jpg"), root.join("usb/album/photo.jpg")).unwrap();
    fs.remove_file(root.join("usb/album/b.jpg")).unwrap();
    fs.write(root.join("docs/c.txt"), b"c").unwrap();
    let plugged = [
        "",
        "docs",
        "docs/a.txt",
        "docs/c.txt",
        "usb",
        "usb/album",
        "usb/album/photo.jpg",
        "usb/new.jpg",
    ];
    settle_on(&app, &repo, &plugged).await;
    reconcile_agrees(&app, &repo, "work on the drive").await;
    assert_eq!(orphan_count(&app, &repo).await, 1, "the one file deleted");

    // Unplugged: nothing changes in the store, nothing is an orphan.
    fs.unmount(root.join("usb")).unwrap();
    mount_table_read_again().await;
    assert_eq!(mounts(&app, &repo).await, [("/usb".to_string(), "\"offline\"".to_string())]);
    reconcile_agrees(&app, &repo, "the drive unplugged").await;
    assert_eq!(tracked_paths(&app, &repo).await, sorted(&plugged));
    assert_eq!(orphan_count(&app, &repo).await, 1);
    assert_eq!(orphans_scanned(&app, &repo).await, Vec::<String>::new());

    // Work while it is away: beside it, in the empty directory it leaves
    // (frozen: ignored by the watcher and the reconcile alike), and on the
    // drive itself, used elsewhere.
    fs.write(root.join("docs/d.txt"), b"d").unwrap();
    fs.write(root.join("usb/beneath.txt"), b"x").unwrap();
    fs.write(store.join("away.jpg"), b"w").unwrap();
    let mut away = plugged.to_vec();
    away.push("docs/d.txt");
    settle_on(&app, &repo, &away).await;
    reconcile_agrees(&app, &repo, "work while the drive is away").await;
    assert_eq!(orphans_scanned(&app, &repo).await, Vec::<String>::new());

    // Plugged back: tracked again as it goes, and what changed while it was
    // away found by a reconcile.
    fs.bind_mount(&store, root.join("usb")).unwrap();
    mount_table_read_again().await;
    assert_eq!(mounts(&app, &repo).await, [("/usb".to_string(), "\"online\"".to_string())]);
    fs.write(root.join("usb/after.jpg"), b"f").unwrap();
    let mut back = away.clone();
    back.push("usb/after.jpg");
    settle_on(&app, &repo, &back).await;
    let result = reconcile(&app, &repo, "the drive plugged back").await;
    assert_eq!(result["created"], 1, "what was written while away: {result}");
    back.push("usb/away.jpg");
    assert_eq!(tracked_paths(&app, &repo).await, sorted(&back));
    fs.rename(root.join("usb/away.jpg"), root.join("usb/album/away.jpg")).unwrap();
    back.pop();
    back.push("usb/album/away.jpg");
    settle_on(&app, &repo, &back).await;
    reconcile_agrees(&app, &repo, "work on the drive plugged back").await;
    assert_eq!(orphan_count(&app, &repo).await, 1);

    fs.unmount(root.join("usb")).unwrap();
    fs.unmount(&store).unwrap();
}

// ── A drive mounted over a directory that holds tracked files ────────────────

async fn a_drive_mounted_over_a_tracked_directory(regime: Regime) {
    let fs = regime.fs();
    let (app, repo, root) = journey_repo("over", regime).await;
    fs.create_dir(root.join("d")).unwrap();
    fs.write(root.join("d/old.txt"), b"o").unwrap();
    settle_on(&app, &repo, &["", "d", "d/old.txt"]).await;
    reconcile_agrees(&app, &repo, "before the mount").await;

    // Hidden, not deleted: nothing says it went, and nothing records it gone.
    fs.mount_tmpfs(root.join("d")).unwrap();
    mount_table_read_again().await;
    reconcile_agrees(&app, &repo, "a drive mounted over it").await;
    assert_eq!(mounts(&app, &repo).await, [("/d".to_string(), "\"online\"".to_string())]);
    fs.write(root.join("d/new.txt"), b"n").unwrap();
    settle_on(&app, &repo, &["", "d", "d/new.txt", "d/old.txt"]).await;
    reconcile_agrees(&app, &repo, "work on the drive").await;
    assert_eq!(orphan_count(&app, &repo).await, 0);
    // The scan reports the hidden file: nothing tells it apart from one
    // deleted from the drive while it was plugged in elsewhere. Reported, not
    // recorded — orphaning it is `mf orphan clear`, asked for.
    assert_eq!(orphans_scanned(&app, &repo).await, ["/d/old.txt"]);

    // Unmounted: the directory is a declared mount point with nothing
    // mounted — frozen, the file that came back into view included.
    fs.unmount(root.join("d")).unwrap();
    mount_table_read_again().await;
    assert_eq!(mounts(&app, &repo).await, [("/d".to_string(), "\"offline\"".to_string())]);
    fs.write(root.join("d/old.txt"), b"changed").unwrap();
    fs.write(root.join("top.txt"), b"t").unwrap();
    settle_on(&app, &repo, &["", "d", "d/new.txt", "d/old.txt", "top.txt"]).await;
    reconcile_agrees(&app, &repo, "the drive unmounted").await;
    assert_eq!(orphans_scanned(&app, &repo).await, Vec::<String>::new());
    assert_eq!(orphan_count(&app, &repo).await, 0);
}

on_both_sources!(
    a_drive_plugged_in_and_out_with_work_going_on,
    a_drive_mounted_over_a_tracked_directory,
);
