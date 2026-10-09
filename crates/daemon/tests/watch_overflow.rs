//! A kernel queue overflow, end to end: the fanotify group's queue fills while
//! the broker is not reading, what follows is lost, and the daemon is told
//! (doc "Watch sources and regimes", "When the broker goes away"). Driven
//! through the simulated kernel, whose queue limit is the kernel's own rule.
//!
//! Its own test binary, because the limit is the whole simulated kernel's —
//! every repository of the process subscribes to the one broker — and a
//! lowered limit would drop the events of whatever test ran beside it.

use axum::http::StatusCode;
use axum::Router;
use serde_json::{json, Value};

mod common;
use common::journey::{
    journey_repo, orphan_count, reconcile, reconcile_agrees, request, root_metarecord, settle_on,
    tracked_paths,
};
use common::{Fs, Regime};

/// The metarecord tracked at `path` (repo-root-relative, no leading slash).
async fn uuid_at(app: &Router, repo: &str, path: &str) -> String {
    let (_, body) = request(
        app,
        "POST",
        &format!("/repos/{repo}/query/fields/resolve-tree"),
        Some(json!({
            "query": {"type": "is_present", "field": "mfr_path"},
            "field": "mfr_path",
        })),
    )
    .await;
    let found = body.as_object().unwrap().iter().find(|(_, paths)| {
        paths.as_array().unwrap().iter().any(|p| p.as_str() == Some(&format!("/{path}")))
    });
    found.unwrap_or_else(|| panic!("nothing tracked at {path}: {body}")).0.clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_kernel_overflow_is_announced_and_closed_by_what_it_names() {
    let regime = Regime::Simulated;
    let fs = regime.fs();
    let Fs::Sim(kernel) = &fs else { unreachable!("the simulated regime") };
    let (app, repo, root) = journey_repo("overflow", regime).await;
    fs.write(root.join("keep.txt"), b"keep").unwrap();
    fs.write(root.join("old.txt"), b"old").unwrap();
    fs.write(root.join("gone.txt"), b"deleted").unwrap();
    settle_on(&app, &repo, &["", "gone.txt", "keep.txt", "old.txt"]).await;
    reconcile_agrees(&app, &repo, "before the overflow").await;
    let old = uuid_at(&app, &repo, "old.txt").await;
    let since = metafolder_daemon::diagnostics::read(0, 1000).next_since;

    // The broker falls behind with a queue of two: the third change on
    // overflows it, and nothing more is queued until it is read.
    {
        let _held = fs.hold();
        kernel.set_max_queued_events(2);
        fs.write(root.join("new1.txt"), b"new1").unwrap();
        fs.write(root.join("new2.txt"), b"new2").unwrap();
        fs.rename(root.join("old.txt"), root.join("renamed.txt")).unwrap(); // Lost.
        fs.remove_file(root.join("gone.txt")).unwrap(); // Lost.
        fs.write(root.join("new3.txt"), b"a third new file").unwrap(); // Lost.
        kernel.set_max_queued_events(metafolder_watchd::sim::DEFAULT_MAX_QUEUED_EVENTS);
    }
    // What came before the overflow is applied; what came after is not.
    settle_on(&app, &repo, &["", "gone.txt", "keep.txt", "new1.txt", "new2.txt", "old.txt"]).await;

    // Announced, for this repository, naming the way back.
    let said: Vec<String> = metafolder_daemon::diagnostics::read(since, 1000)
        .entries
        .into_iter()
        .filter(|e| e.scope == "watcher" && e.repo.as_deref() == Some(repo.as_str()))
        .map(|e| e.message)
        .collect();
    let notice = said
        .iter()
        .find(|m| m.contains("mf reconcile"))
        .unwrap_or_else(|| panic!("the overflow is announced: {said:?}"));
    // A reconcile picks up the new files, but neither a deletion (it never
    // writes `nothing`) nor, mostly, a move: the notice names the orphan scan.
    assert!(notice.contains("mf orphan detect"), "{notice}");

    // The watcher carries on after the gap.
    fs.write(root.join("after.txt"), b"after").unwrap();
    settle_on(
        &app,
        &repo,
        &["", "after.txt", "gone.txt", "keep.txt", "new1.txt", "new2.txt", "old.txt"],
    )
    .await;

    // The reconcile closes what it can: the lost creation. The lost rename is
    // a file the watcher tracked, so no full hash is stored: the reconcile
    // offers it as a candidate for the record left at the old path rather
    // than following it (doc "Moves and candidates")…
    let result = reconcile(&app, &repo, "after the overflow").await;
    assert_eq!(result["created"], 1, "{result}");
    assert_eq!(result["moved"], 0, "{result}");
    assert_eq!(
        result["candidates"],
        json!([{
            "metarecord_uuid": old,
            "stale_path": "/old.txt",
            "matches": [{"path": "/renamed.txt", "fingerprint": "size"}],
        }]),
    );
    // …confirmed by writing the record's position.
    let parent = root_metarecord(&app, &repo).await;
    let (status, body) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{old}/fields/mfr_path"),
        Some(json!({
            "value": {"type": "tree_ref", "value": {"parent": parent, "name": "renamed.txt"}},
            "force": true,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "confirming the candidate: {body}");
    assert_eq!(uuid_at(&app, &repo, "renamed.txt").await, old, "the rename kept its record");

    // The lost deletion is the orphan scan's: the one record whose file is gone.
    let (status, scan) = request(&app, "POST", &format!("/repos/{repo}/orphans/scan"), None).await;
    assert_eq!(status, StatusCode::OK, "{scan}");
    assert_eq!(scan["count"], 1, "{scan}");
    assert_eq!(scan["orphans"][0]["stale_path"], "/gone.txt", "{scan}");
    let gone: Value = scan["orphans"][0]["uuid"].clone();
    let (status, cleared) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/orphans/clear"),
        Some(json!({"uuids": [gone]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");

    // And the store says what the disk says.
    assert_eq!(
        tracked_paths(&app, &repo).await,
        ["", "after.txt", "keep.txt", "new1.txt", "new2.txt", "new3.txt", "renamed.txt"]
    );
    reconcile_agrees(&app, &repo, "gap closed").await;
    assert_eq!(orphan_count(&app, &repo).await, 1, "the deleted file's record is preserved");
}
