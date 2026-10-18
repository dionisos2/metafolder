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

// ── One drive mounted at two points of a repository ──────────────────────────

/// What `POST /watch/check` says of `path`: `(watched, reason)`.
async fn watch_check(app: &Router, repo: &str, path: &str) -> (bool, String) {
    let (_, body) = request(
        app,
        "POST",
        &format!("/repos/{repo}/watch/check"),
        Some(serde_json::json!({"paths": [path]})),
    )
    .await;
    let r = &body["results"][0];
    (
        r["watched"].as_bool().unwrap_or_else(|| panic!("watch check: {body}")),
        r["reason"].to_string(),
    )
}

async fn a_drive_mounted_at_two_points(regime: Regime) {
    let fs = regime.fs();
    let (app, repo, root) = journey_repo("twice", regime).await;
    fs.create_dir(root.join("a")).unwrap();
    fs.create_dir(root.join("b")).unwrap();
    settle_on(&app, &repo, &["", "a", "b"]).await;
    let store = drive(&fs, "twice");
    std::fs::write(store.join("x.jpg"), b"x").unwrap();
    fs.bind_mount(&store, root.join("a")).unwrap();
    fs.bind_mount(&store, root.join("b")).unwrap();
    mount_table_read_again().await;

    // Tracked once, where it was mounted first: the second mount is an
    // entry, never walked — as a symlinked directory (doc "Second mounts").
    let result = reconcile(&app, &repo, "the drive at two points").await;
    assert_eq!(result["created"], 1, "{result}");
    assert_eq!(tracked_paths(&app, &repo).await, sorted(&["", "a", "a/x.jpg", "b"]));
    assert_eq!(watch_check(&app, &repo, "/b/x.jpg").await, (false, "\"second_mount\"".into()));
    reconcile_agrees(&app, &repo, "the drive at two points").await;

    // Written through the second point: recorded where the drive is tracked.
    fs.write(root.join("b/y.jpg"), b"y").unwrap();
    settle_on(&app, &repo, &["", "a", "a/x.jpg", "a/y.jpg", "b"]).await;
    reconcile_agrees(&app, &repo, "written through the second point").await;
    fs.rename(root.join("b/x.jpg"), root.join("b/z.jpg")).unwrap();
    settle_on(&app, &repo, &["", "a", "a/y.jpg", "a/z.jpg", "b"]).await;
    reconcile_agrees(&app, &repo, "renamed through the second point").await;

    // The first point unmounted: frozen there, and the second is the drive's
    // one mount now — live, found by a reconcile as a drive plugged in is.
    fs.unmount(root.join("a")).unwrap();
    mount_table_read_again().await;
    let result = reconcile(&app, &repo, "the first point unmounted").await;
    assert_eq!(result["created"], 2, "{result}");
    fs.write(root.join("b/w.jpg"), b"w").unwrap();
    let after = ["", "a", "a/y.jpg", "a/z.jpg", "b", "b/w.jpg", "b/y.jpg", "b/z.jpg"];
    settle_on(&app, &repo, &after).await;
    reconcile_agrees(&app, &repo, "the first point unmounted").await;
    assert_eq!(orphan_count(&app, &repo).await, 0);
    assert_eq!(orphans_scanned(&app, &repo).await, Vec::<String>::new());

    fs.unmount(root.join("b")).unwrap();
    fs.unmount(&store).unwrap();
}

// ── One drive in two repositories ────────────────────────────────────────────

async fn a_drive_in_two_repositories(regime: Regime) {
    let fs = regime.fs();
    let (app1, repo1, root1) = journey_repo("tworepos1", regime).await;
    let (app2, repo2, root2) = journey_repo("tworepos2", regime).await;
    let store = drive(&fs, "shared");
    std::fs::write(store.join("photo.jpg"), b"p").unwrap();
    for (app, repo, root) in [(&app1, &repo1, &root1), (&app2, &repo2, &root2)] {
        fs.create_dir(root.join("usb")).unwrap();
        settle_on(app, repo, &["", "usb"]).await;
        fs.bind_mount(&store, root.join("usb")).unwrap();
    }
    mount_table_read_again().await;
    for (app, repo) in [(&app1, &repo1), (&app2, &repo2)] {
        let result = reconcile(app, repo, "the drive plugged in").await;
        assert_eq!(result["created"], 1, "{result}");
    }

    // Worked on through either repository: both hear of it.
    fs.write(root2.join("usb/new.jpg"), b"n").unwrap();
    fs.rename(root1.join("usb/photo.jpg"), root1.join("usb/kept.jpg")).unwrap();
    for (app, repo) in [(&app1, &repo1), (&app2, &repo2)] {
        settle_on(app, repo, &["", "usb", "usb/kept.jpg", "usb/new.jpg"]).await;
        reconcile_agrees(app, repo, "worked on through either repository").await;
        assert_eq!(orphan_count(app, repo).await, 0);
    }

    fs.unmount(root1.join("usb")).unwrap();
    fs.unmount(root2.join("usb")).unwrap();
    fs.unmount(&store).unwrap();
}

// ── A filesystem the broker cannot cover ─────────────────────────────────────

/// What `GET /watch` lists as the mounts the broker could not cover, by path.
async fn uncovered_mounts(app: &Router, repo: &str) -> Vec<String> {
    let (_, body) = request(app, "GET", &format!("/repos/{repo}/watch"), None).await;
    let Some(list) = body["uncovered"]["mounts"].as_array() else { return Vec::new() };
    list.iter().map(|m| m["path"].as_str().unwrap_or("?").to_string()).collect()
}

/// Waits until a change at `path` would be recorded.
async fn until_watched(app: &Router, repo: &str, path: &str) {
    for _ in 0..100 {
        if watch_check(app, repo, path).await.0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{path} is never watched: {:?}", watch_check(app, repo, path).await);
}

/// A filesystem mounted beneath the root that no fanotify mark can cover — a
/// FUSE one with a zero fsid, gocryptfs — is watched with inotify, inside it
/// only (doc "Uncovered mounts"). What is written there is written with
/// `std::fs`: the kernel queues nothing for it, as for a filesystem nobody
/// marked.
async fn a_filesystem_the_broker_cannot_cover(regime: Regime) {
    let fs = regime.fs();
    let (app, repo, root) = journey_repo("uncovered", regime).await;
    fs.create_dir(root.join("enc")).unwrap();
    fs.write(root.join("top.txt"), b"t").unwrap();
    settle_on(&app, &repo, &["", "enc", "top.txt"]).await;

    fs.mount_uncoverable(root.join("enc")).unwrap();
    mount_table_read_again().await;
    if regime == Regime::Simulated {
        assert_eq!(uncovered_mounts(&app, &repo).await, ["/enc"]);
    }
    until_watched(&app, &repo, "/enc/a.txt").await;

    std::fs::write(root.join("enc/a.txt"), b"a").unwrap();
    std::fs::create_dir(root.join("enc/sub")).unwrap();
    std::fs::write(root.join("enc/sub/b.txt"), b"b").unwrap();
    settle_on(&app, &repo, &["", "enc", "enc/a.txt", "enc/sub", "enc/sub/b.txt", "top.txt"]).await;
    // A directory that arrived is watched in turn.
    std::fs::rename(root.join("enc/a.txt"), root.join("enc/sub/c.txt")).unwrap();
    fs.write(root.join("beside.txt"), b"s").unwrap();
    let after = ["", "beside.txt", "enc", "enc/sub", "enc/sub/b.txt", "enc/sub/c.txt", "top.txt"];
    settle_on(&app, &repo, &after).await;
    reconcile_agrees(&app, &repo, "work in the uncovered mount").await;
    assert_eq!(orphan_count(&app, &repo).await, 0);

    // Unmounted: nothing left to watch with inotify.
    fs.unmount(root.join("enc")).unwrap();
    mount_table_read_again().await;
    if regime == Regime::Simulated {
        for _ in 0..50 {
            if uncovered_mounts(&app, &repo).await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(uncovered_mounts(&app, &repo).await, Vec::<String>::new());
    }
}

on_both_sources!(
    a_drive_plugged_in_and_out_with_work_going_on,
    a_drive_mounted_over_a_tracked_directory,
    a_drive_mounted_at_two_points,
    a_drive_in_two_repositories,
    a_filesystem_the_broker_cannot_cover,
);
