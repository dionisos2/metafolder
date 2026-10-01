//! Coordinated navigation of the event log (doc "Filesystem coordination",
//! doc "Revert"), shared by the CLI (`mf log rollback`/`revert`/`undo`/`redo`)
//! and the GUI (`log:undo`, `log:redo`, the log panel).
//!
//! The daemon owns the metadata and never touches a file; it says, operation by
//! operation, what crossing it requires on disk (`filesystem.action`: move a
//! file, bring content back from the trash-bin, send it back there). This module
//! is the client half: it performs those actions and tells the daemon what it
//! managed, over [`DaemonClient`] and [`crate::trash`]. Written once here, so the
//! GUI and the CLI cannot disagree about what an undo does to a file.
//!
//! What stays each client's own goes through [`NavigationUi`]: how a move
//! decision left to the user is asked (the CLI reads stdin, the GUI never asks),
//! and where the notes along the way are shown.

use crate::daemon_client::{with_query, DaemonClient, DaemonError};
use crate::trash::{Reason, TrashDir, TrashEntry, TrashError};
use serde_json::{json, Value as Json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A navigation that failed, as the message to show.
#[derive(Debug, Clone)]
pub struct NavError(pub String);

impl std::fmt::Display for NavError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NavError {}

impl From<DaemonError> for NavError {
    fn from(e: DaemonError) -> Self {
        NavError(e.message)
    }
}

impl From<TrashError> for NavError {
    fn from(e: TrashError) -> Self {
        NavError(e.0)
    }
}

/// What to do with a `move` step (doc "Rolling back").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Apply,
    Skip,
    Abort,
    /// Leave it to the user, through [`NavigationUi::ask_move`].
    Ask,
}

/// The move policies, for a file that is where the log says (`on_available`)
/// and for one that is not (`on_unavailable`).
#[derive(Debug, Clone, Copy)]
pub struct MovePolicies {
    pub on_available: Policy,
    pub on_unavailable: Policy,
}

/// What a navigation needs from the front end driving it.
pub trait NavigationUi {
    /// Decides a move the policies left to the user ([`Policy::Ask`]). Never
    /// answers `Ask`.
    fn ask_move(&self, from: &str, to: &str, available: bool) -> Result<Policy, NavError>;
    /// Something the user should hear about: a file moved or left, content
    /// brought back or not.
    fn note(&self, message: &str);
    /// A navigation of `total` operations is starting (the CLI announces it).
    fn navigating(&self, _total: usize) {}
}

/// A repository, as a navigation needs it: the daemon, and the trash-bin the
/// daemon reports (`internal_dir`, doc "Trash").
pub struct Repo<'a> {
    pub client: &'a dyn DaemonClient,
    /// The repository uuid (hex).
    pub repo: String,
    /// The repository root, where the log's paths are.
    pub root: PathBuf,
    pub trash: TrashDir,
}

impl<'a> Repo<'a> {
    /// Locates the repository's trash-bin through `GET /repos/:repo`.
    pub fn open(client: &'a dyn DaemonClient, repo: &str) -> Result<Self, NavError> {
        let info = client.get(&format!("/repos/{repo}"))?;
        let internal = info["internal_dir"]
            .as_str()
            .ok_or_else(|| NavError("daemon did not report the repo internal_dir".into()))?;
        let root = info["root"]
            .as_str()
            .ok_or_else(|| NavError("daemon did not report the repo root".into()))?;
        Ok(Self {
            client,
            repo: repo.to_string(),
            root: PathBuf::from(root),
            trash: TrashDir::new(Path::new(internal).join("trash")),
        })
    }

    fn base(&self) -> String {
        format!("/repos/{}", self.repo)
    }
}

// ── Rollback ─────────────────────────────────────────────────────────────────

/// The query-parameter form of a navigation target (`{"id": N}`,
/// `{"timestamp": T}`, `{"label": L}` or `{"prev_revision": true}`), for `GET
/// /rollback/plan` and `plan/summary`.
pub fn target_query(target: &Json) -> Vec<(&'static str, String)> {
    if let Some(id) = target["id"].as_i64() {
        vec![("target_id", id.to_string())]
    } else if let Some(ts) = target["timestamp"].as_i64() {
        vec![("target_timestamp", ts.to_string())]
    } else if let Some(label) = target["label"].as_str() {
        vec![("target_label", label.to_string())]
    } else {
        vec![("target_prev_revision", "true".into())]
    }
}

/// The stored op types whose navigation may need a decision about the
/// filesystem — the reading of a summary from a daemon that does not count
/// `filesystem_steps` itself. `delete_metarecord` is in the list although only
/// the ones a *trashing* wrote carry a file: types cannot say which revision
/// wrote them, so the whole type takes the careful road.
const FILESYSTEM_OPS: &[&str] =
    &["file_moved", "file_deleted", "file_modified", "delete_metarecord"];

/// Whether a plan summary's operations all rewind inside the database, with
/// nothing on disk to decide. Those can be navigated in one atomic call
/// (`POST /rollback`) instead of one round-trip — and one transaction — per
/// operation.
///
/// This is what "back" costs in a classification walk (doc "Script sessions"): a "yes" on a folder
/// writes one operation per file under it, so
/// taking it back stepped a thousand times over the network to undo a single
/// keypress. The daemon counts the steps with a file action
/// (`filesystem_steps`); failing that the op types are read, and an unreadable
/// summary is *not* metadata-only: the careful road is the one that is always
/// correct.
pub fn rewinds_in_the_database_alone(summary: &Json) -> bool {
    if let Some(steps) = summary["filesystem_steps"].as_u64() {
        return steps == 0;
    }
    summary["by_type"]
        .as_object()
        .is_some_and(|by_type| !by_type.keys().any(|t| FILESYSTEM_OPS.contains(&t.as_str())))
}

/// What a rollback did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Navigated {
    /// Operations on the way to the target.
    pub total: usize,
    /// Operations actually navigated.
    pub processed: usize,
}

/// Navigates HEAD to `target` (a `{"id"|"timestamp"|"label"|"prev_revision"}`
/// object): in one atomic call when nothing on the way touches a file,
/// otherwise through the coordinated protocol, performing each step's file
/// action — and always releasing the lock (`abort`) on an error.
pub fn rollback(
    repo: &Repo<'_>,
    target: &Json,
    policies: &MovePolicies,
    ui: &dyn NavigationUi,
) -> Result<Navigated, NavError> {
    let base = repo.base();
    let client = repo.client;
    let summary =
        client.get(&with_query(&format!("{base}/rollback/plan/summary"), &target_query(target)))?;
    let total = summary["total_operations"].as_u64().unwrap_or(0) as usize;
    if total == 0 {
        return Ok(Navigated { total: 0, processed: 0 });
    }
    ui.navigating(total);
    let body = json!({"target": target});

    if rewinds_in_the_database_alone(&summary) {
        let result = client.post(&format!("{base}/rollback"), &body)?;
        let processed = result["operations_unapplied"].as_u64().unwrap_or(0)
            + result["operations_applied"].as_u64().unwrap_or(0);
        return Ok(Navigated { total, processed: processed as usize });
    }

    // The trash-bin catches any file a `move` step would overwrite, so no byte
    // is ever lost by rollback (doc "Trash"). Its entries also let us point out
    // content that a deletion step "lost" but that is in fact recoverable.
    let mut entries = repo.trash.entries().unwrap_or_default();
    // Metarecords whose content a directory blob has already brought back this
    // navigation, so their own steps apply the inverse, not skip.
    let mut restored: HashSet<String> = HashSet::new();
    // Metarecords a trashing redone has already sent back, with their subtree.
    let mut retrashed: HashSet<String> = HashSet::new();

    let start = client.post(&format!("{base}/rollback/start"), &body)?;
    let mut op = start["op"].clone();
    let mut processed = 0usize;
    let outcome = (|| -> Result<(), NavError> {
        while !op.is_null() {
            let skip = match op["filesystem"]["action"].as_str() {
                Some("move") => decide_move(&op, policies, &repo.trash, ui)?,
                // The content comes back from the trash: a file the watcher saw
                // go (matched by metarecord and version), or a metarecord a
                // trashing took (matched by the metarecord alone — an undo
                // consumes the entry and a redo makes a new one, so no id
                // recorded at trash time would still name it).
                Some("restore_content") => decide_deleted(
                    &op,
                    &repo.trash,
                    &mut entries,
                    &mut restored,
                    ui,
                    takes_a_whole_metarecord(&op),
                )?,
                // Before the step, while the metarecord is still there to read.
                Some("trash_content") => {
                    retrash(repo, &op, &mut retrashed, ui)?;
                    false
                }
                _ => false,
            };
            let step_body = if skip { json!({"skip": true}) } else { json!({}) };
            let resp = client.post(&format!("{base}/rollback/step"), &step_body)?;
            processed += 1;
            op = resp["op"].clone();
        }
        Ok(())
    })();
    match outcome {
        Ok(()) => Ok(Navigated { total, processed }),
        Err(err) => {
            // Release the lock; the moves already executed stay (the caller
            // hears what happened through the notes).
            let _ = client.post(&format!("{base}/rollback/abort"), &json!({}));
            Err(err)
        }
    }
}

/// Whether the operation creates or deletes a whole metarecord — a trashing's
/// kind of file action, as opposed to a file the watcher saw deleted or
/// modified.
fn takes_a_whole_metarecord(op: &Json) -> bool {
    matches!(op["op_type"].as_str(), Some("delete_metarecord") | Some("create_metarecord"))
}

/// Decides a `move` step, executing the `mv` for the apply policy. Returns
/// whether to `skip` (no filesystem move) when calling `step`.
pub fn decide_move(
    op: &Json,
    policies: &MovePolicies,
    trash: &TrashDir,
    ui: &dyn NavigationUi,
) -> Result<bool, NavError> {
    let from = op["from"].as_str().unwrap_or_default();
    let to = op["to"].as_str().unwrap_or_default();
    // `path_present`, not `exists()`: a broken symlink is a file that is
    // there, and one this step can move (see `crate::fsentry::path_present`).
    let available = crate::fsentry::path_present(Path::new(from));
    let mut policy = if available { policies.on_available } else { policies.on_unavailable };
    if policy == Policy::Ask {
        policy = ui.ask_move(from, to, available)?;
    }
    match policy {
        // Apply: the metadata follows the navigation to `to` (via `step {}`).
        // Move the file there when it is present; when it is gone there is
        // nothing to move — the metadata still follows the rollback, keeping
        // the recorded path rather than rewinding to a location the file is
        // not at (doc "Rolling back"; review #6).
        Policy::Apply => {
            if available {
                let dest = Path::new(to);
                // A directory at `to` can be neither overwritten by an `mv`
                // (rename would fail) nor trashed in its place without taking a
                // whole subtree the log says nothing about. Rather than abort
                // the whole navigation, skip just this step (the metadata
                // rewinds, the file stays put) and say so.
                if crate::fsentry::is_real_dir(dest) {
                    ui.note(&format!(
                        "skipped move {from} -> {to}: a directory occupies the destination"
                    ));
                    return Ok(true);
                }
                // If `to` is occupied by a file, the mv would overwrite it —
                // trash the occupant first so its content survives (doc "Trash").
                // The occupant's own metarecord is unknown here (the op's
                // entity_uuid names the *moved* record, not this file), so the
                // entry records only the causing revision, not a metarecord.
                //
                // Note: trash-then-rename is not atomic. If the rename fails
                // after trashing (e.g. `from` vanished, or a cross-device
                // from→to), `to` is left empty with its content recoverable in
                // the trash, and the navigation aborts mid-way.
                if crate::fsentry::path_present(dest) {
                    let entry =
                        trash.trash_path(dest, Reason::Rollback, op["id"].as_i64(), None, None)?;
                    ui.note(&format!("trashed {to} (id {}) before overwrite", entry.id));
                }
                std::fs::rename(from, to)
                    .map_err(|e| NavError(format!("mv {from} -> {to} failed: {e}")))?;
                ui.note(&format!("moved {from} -> {to}"));
            } else {
                ui.note(&format!("kept rolled-back path for {to} (source {from} is gone)"));
            }
            Ok(false)
        }
        Policy::Skip => {
            ui.note(&format!("skipped move {from} -> {to}"));
            Ok(true)
        }
        Policy::Abort => Err(NavError("rollback aborted by move policy".into())),
        Policy::Ask => Err(NavError("the move policy was left undecided".into())),
    }
}

/// Decides a step whose content lives in the trash-bin (doc "Trash, undo and
/// redo"). When the trash holds it, the file is put back and `false` (no skip) is returned so the daemon applies the real inverse;
/// otherwise the step is skipped (metadata rewinds) and, if some content for
/// the record is trashed, a recovery hint is noted.
///
/// A trash entry covers the step's metarecord when it *is* the entry's
/// metarecord, or a descendant recorded in its subtree (a trashed directory):
/// restoring the directory blob brings back the whole subtree's content at
/// once, whichever order the cascade's ops are navigated in. For a file the
/// watcher saw go, the entry must also hold the version the record had before
/// the revision being undone (`entity_version_before_revision`, present only on
/// inverse steps): the precise per-file correlation. `by_metarecord_only`
/// drops that condition, for a metarecord a trashing deleted outright — the
/// live entry holding it is unambiguous, since nothing can trash it again
/// until it comes back.
pub fn decide_deleted(
    op: &Json,
    trash: &TrashDir,
    entries: &mut Vec<TrashEntry>,
    restored: &mut HashSet<String>,
    ui: &dyn NavigationUi,
    by_metarecord_only: bool,
) -> Result<bool, NavError> {
    let entity = op["entity_uuid"].as_str().unwrap_or_default();

    // Content already brought back by a directory blob restored earlier in this
    // navigation: the file is on disk, so apply the real inverse — don't skip.
    if restored.contains(entity) {
        return Ok(false);
    }

    // The version the record held before the *whole* revision: one event can
    // write several fields (orphaning writes `mfr_path` and `mfr_path_old`), so
    // each op restores to its own intermediate version, while the trash entry
    // recorded the version at the moment the file was trashed — the
    // pre-revision one. Every op of the revision therefore reaches the same
    // decision. Older daemons expose only the per-op version.
    let version = op["entity_version_before_revision"]
        .as_u64()
        .or_else(|| op["entity_version_before"].as_u64());
    let pos = entries.iter().position(|e| {
        let is_top = e.metarecord.as_deref() == Some(entity)
            && (by_metarecord_only || version.is_some_and(|v| e.version == Some(v)));
        let is_descendant =
            e.metarecord.as_deref() != Some(entity) && e.subtree.iter().any(|n| n.uuid == entity);
        is_top || is_descendant
    });
    if let Some(pos) = pos {
        let id = entries[pos].id.clone();
        let covered: Vec<String> = entries[pos].subtree.iter().map(|n| n.uuid.clone()).collect();
        match trash.restore(&id) {
            Ok(path) => {
                entries.remove(pos);
                // Every metarecord the blob restored is now recoverable, so
                // their steps apply the real inverse too.
                restored.extend(covered);
                restored.insert(entity.to_string());
                ui.note(&format!("restored {} from the trash", path.display()));
                // Apply the real inverse (`step {}`): the daemon restores the
                // metadata to match the file now in place.
                return Ok(false);
            }
            // The file is not restorable (e.g. the path is occupied); fall back
            // to skipping, so the metadata stays truthful.
            Err(e) => {
                ui.note(&format!("note: could not auto-restore from the trash ({e}); skipping"))
            }
        }
    }
    if let Some(hint) = trash_recovery_hint(entries, entity) {
        ui.note(&format!("note: {hint}"));
    }
    Ok(true)
}

/// The repository-relative path of a metarecord's file (`mfr_path`), `None`
/// when it has none or the metarecord is gone.
fn path_of(repo: &Repo<'_>, uuid: &str) -> Result<Option<String>, NavError> {
    let path = format!("{}/metarecords/{uuid}/fields/mfr_path/resolve-tree", repo.base());
    match repo.client.get(&path) {
        Ok(resp) => Ok(resp["paths"]
            .as_array()
            .and_then(|paths| paths.first())
            .and_then(Json::as_str)
            .map(str::to_string)),
        Err(e) if e.is_not_found() => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Performs a `trash_content` step: a metarecord a trashing took is going
/// again, so its file goes back into the trash-bin, as a trashing does it —
/// captured first (the metarecord, its subtree and its ancestors, with their
/// fields, doc "What trashing does"), into a *new* entry, the
/// undo having consumed the old one. Runs before the daemon deletes the
/// metarecord, while there is still something to read; the watcher is held
/// for the whole navigation, so the file's disappearance orphans nothing.
///
/// A metarecord already covered by an earlier step's capture (a file inside a
/// directory sent back whole) needs nothing more. One whose file is not on disk
/// has nothing to send: the metadata is deleted all the same, and the note says
/// the bytes were not there.
fn retrash(
    repo: &Repo<'_>,
    op: &Json,
    covered: &mut HashSet<String>,
    ui: &dyn NavigationUi,
) -> Result<(), NavError> {
    let entity = op["entity_uuid"].as_str().unwrap_or_default();
    if covered.contains(entity) {
        return Ok(());
    }
    let record = match repo.client.get(&format!("{}/metarecords/{entity}", repo.base())) {
        Ok(record) => record,
        Err(e) if e.is_not_found() => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let Some(rel) = path_of(repo, entity)? else { return Ok(()) };
    let abs = repo.root.join(rel.trim_start_matches('/'));
    if !crate::fsentry::path_present(&abs) {
        ui.note(&format!("{} is not on disk: nothing to send back to the trash", abs.display()));
        return Ok(());
    }
    let subtree = crate::trash::capture_nodes(repo.client, &repo.repo, &record, &rel)?;
    let entry = repo.trash.trash_path(
        &abs,
        Reason::Manual,
        op["id"].as_i64(),
        Some(entity.to_string()),
        record["version"].as_u64(),
    )?;
    covered.insert(entity.to_string());
    covered.extend(subtree.iter().filter(|n| n.trashed).map(|n| n.uuid.clone()));
    repo.trash.attach_subtree(&entry.id, subtree)?;
    ui.note(&format!("trashed {} again (id {})", abs.display(), entry.id));
    Ok(())
}

/// If the trash holds content for `metarecord` (e.g. a prior `mf trash -f`),
/// the hint to show when its step is skipped — bridging the log's "content
/// gone" assumption with the bytes that actually survive in the trash
/// (doc "Trash"). `None` when nothing matches.
pub fn trash_recovery_hint(entries: &[TrashEntry], metarecord: &str) -> Option<String> {
    if metarecord.is_empty() {
        return None;
    }
    let ids: Vec<&str> = entries
        .iter()
        .filter(|e| e.metarecord.as_deref() == Some(metarecord))
        .map(|e| e.id.as_str())
        .collect();
    (!ids.is_empty()).then(|| {
        format!(
            "content for this record is in the trash — recover with: mf trash restore {}",
            ids.join(" | ")
        )
    })
}

// ── Revert ───────────────────────────────────────────────────────────────────

/// The query-parameter form of a revert target (`{"rev_id": N|"head"}` or
/// `{"op_ids": [...]}`), for `GET /revert/plan`.
pub fn revert_query(target: &Json, with_dependents: bool) -> Vec<(&'static str, String)> {
    let mut q = Vec::new();
    match target["op_ids"].as_array().filter(|ids| !ids.is_empty()) {
        Some(ids) => {
            let ids: Vec<String> =
                ids.iter().filter_map(Json::as_i64).map(|i| i.to_string()).collect();
            q.push(("target_op_ids", ids.join(",")));
        }
        None => {
            let rev = match &target["rev_id"] {
                Json::Number(n) => n.to_string(),
                _ => "head".into(),
            };
            q.push(("target_rev_id", rev));
        }
    }
    if with_dependents {
        q.push(("with_dependents", "true".into()));
    }
    q
}

/// The revert plan for `target` (`GET /revert/plan`), writing nothing.
pub fn revert_plan(
    repo: &Repo<'_>,
    target: &Json,
    with_dependents: bool,
) -> Result<Json, NavError> {
    let path =
        with_query(&format!("{}/revert/plan", repo.base()), &revert_query(target, with_dependents));
    Ok(repo.client.get(&path)?)
}

/// A revert to write.
pub struct RevertRequest<'r> {
    /// `{"rev_id": N|"head"}` or `{"op_ids": [...]}`.
    pub target: &'r Json,
    pub with_dependents: bool,
    /// Leave out the operations needing a file action instead of performing it.
    pub metadata_only: bool,
    pub label: Option<&'r str>,
}

/// Writes the revert `plan` (as [`revert_plan`] answered it) describes: in one
/// call when nothing needs a file action (or `metadata_only` leaves those out),
/// otherwise through the coordinated protocol — `start` takes the lock, the
/// file actions are performed, `commit` writes what they managed, and any error
/// is an `abort` that leaves the log untouched. Returns the daemon's answer
/// (`revision`, `reverted_operations`, `skipped_operations`).
pub fn revert(
    repo: &Repo<'_>,
    request: &RevertRequest<'_>,
    plan: &Json,
    policies: &MovePolicies,
    ui: &dyn NavigationUi,
) -> Result<Json, NavError> {
    let base = repo.base();
    let client = repo.client;
    let needs_fs = plan["requires_lock"] == json!(true) && !request.metadata_only;
    if !needs_fs {
        let mut body =
            json!({"target": request.target, "with_dependents": request.with_dependents});
        if request.metadata_only {
            body["skip_filesystem"] = json!(true);
        }
        if let Some(label) = request.label {
            body["label"] = json!(label);
        }
        return Ok(client.post(&format!("{base}/revert"), &body)?);
    }

    let start_body = json!({"target": request.target, "with_dependents": request.with_dependents});
    let started = client.post(&format!("{base}/revert/start"), &start_body)?;
    let mut entries = repo.trash.entries().unwrap_or_default();
    let mut restored: HashSet<String> = HashSet::new();
    let mut retrashed: HashSet<String> = HashSet::new();

    let outcome = (|| -> Result<Vec<i64>, NavError> {
        // The trashings to redo, shallowest first: a revert's operations are
        // not ordered parent first, and a directory has to be captured before
        // its files, so that it goes to the trash whole, in one entry.
        let mut to_retrash: Vec<(usize, &Json)> = Vec::new();
        for op in started["operations"].as_array().into_iter().flatten() {
            if op["filesystem"]["action"] == "trash_content" {
                let entity = op["entity_uuid"].as_str().unwrap_or_default();
                let depth = path_of(repo, entity)?.map_or(0, |p| p.matches('/').count());
                to_retrash.push((depth, op));
            }
        }
        to_retrash.sort_by_key(|(depth, _)| *depth);
        for (_, op) in to_retrash {
            retrash(repo, op, &mut retrashed, ui)?;
        }

        let mut apply = Vec::new();
        for op in started["operations"].as_array().into_iter().flatten() {
            let Some(id) = op["id"].as_i64() else { continue };
            // `decide_move`/`decide_deleted` answer "skip?" — which for a revert
            // means "leave this operation out", the whole thing a rollback needs
            // a restoration operation for.
            let leave_out = match op["filesystem"]["action"].as_str() {
                Some("move") => {
                    let step =
                        json!({"from": op["filesystem"]["from"], "to": op["filesystem"]["to"]});
                    decide_move(&step, policies, &repo.trash, ui)?
                }
                Some("restore_content") => decide_deleted(
                    op,
                    &repo.trash,
                    &mut entries,
                    &mut restored,
                    ui,
                    takes_a_whole_metarecord(op),
                )?,
                // Already sent back to the trash above.
                _ => false,
            };
            if !leave_out {
                apply.push(id);
            }
        }
        Ok(apply)
    })();
    let apply = match outcome {
        Ok(apply) => apply,
        Err(err) => {
            let _ = client.post(&format!("{base}/revert/abort"), &json!({}));
            return Err(err);
        }
    };
    let mut body = json!({"apply": apply});
    if let Some(label) = request.label {
        body["label"] = json!(label);
    }
    match client.post(&format!("{base}/revert/commit"), &body) {
        Ok(resp) => Ok(resp),
        Err(err) => {
            let _ = client.post(&format!("{base}/revert/abort"), &json!({}));
            Err(err.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A front end that never asks and remembers what it was told.
    #[derive(Default)]
    struct Quiet {
        notes: RefCell<Vec<String>>,
    }
    impl NavigationUi for Quiet {
        fn ask_move(&self, _: &str, _: &str, _: bool) -> Result<Policy, NavError> {
            Err(NavError("nobody to ask".into()))
        }
        fn note(&self, message: &str) {
            self.notes.borrow_mut().push(message.to_string());
        }
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("nav_{tag}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn move_op(from: &Path, to: &Path) -> Json {
        json!({"op_type": "move_file", "from": from, "to": to,
               "filesystem": {"action": "move"}})
    }

    fn policies(on_available: Policy, on_unavailable: Policy) -> MovePolicies {
        MovePolicies { on_available, on_unavailable }
    }

    /// "Back" in a classification walk (doc "Script sessions") undoes a
    /// whole subtree of tag writes. Nothing in that touches a file, so the
    /// navigation goes in one atomic call — one round-trip per record is what
    /// made going back take minutes.
    #[test]
    fn a_plan_with_no_file_operation_is_navigated_in_one_call() {
        // The daemon's own count, when it gives one.
        assert!(rewinds_in_the_database_alone(&json!({"filesystem_steps": 0,
            "by_type": {"delete_metarecord": 3}})));
        assert!(!rewinds_in_the_database_alone(&json!({"filesystem_steps": 1,
            "by_type": {"set_field": 3}})));

        // Otherwise the op types.
        let plan = |by_type| json!({"total_operations": 3, "by_type": by_type});
        assert!(rewinds_in_the_database_alone(&plan(json!({"set_field": 2, "append_field": 1}))));
        assert!(rewinds_in_the_database_alone(&plan(json!({"create_metarecord": 3}))));
        for op_type in ["file_moved", "file_deleted", "file_modified", "delete_metarecord"] {
            let by_type = json!({"set_field": 1, op_type: 1});
            assert!(!rewinds_in_the_database_alone(&plan(by_type)), "{op_type}");
        }
        // Nothing to read: the careful road, never the fast one.
        assert!(!rewinds_in_the_database_alone(&json!({})));
    }

    #[test]
    fn targets_have_their_query_forms() {
        assert_eq!(target_query(&json!({"id": 4})), vec![("target_id", "4".to_string())]);
        assert_eq!(
            target_query(&json!({"prev_revision": true})),
            vec![("target_prev_revision", "true".to_string())]
        );
        assert_eq!(
            revert_query(&json!({"op_ids": [3, 5]}), true),
            vec![("target_op_ids", "3,5".to_string()), ("with_dependents", "true".to_string())]
        );
        assert_eq!(
            revert_query(&json!({"rev_id": "head"}), false),
            vec![("target_rev_id", "head".to_string())]
        );
        assert_eq!(
            revert_query(&json!({"rev_id": 7}), false),
            vec![("target_rev_id", "7".to_string())]
        );
    }

    // `apply` on a gone file: no `mv` is attempted (nothing to move) and the
    // step is a plain `step {}` — the metadata follows the rollback instead of
    // erroring or rewinding to a location the file is not at (review #6).
    #[test]
    fn apply_on_a_gone_file_keeps_the_rolled_back_path_without_moving() {
        let tmp = scratch("gone");
        let (from, to) = (tmp.join("gone.txt"), tmp.join("target.txt"));
        let trash = TrashDir::new(tmp.join("trash"));
        let skip = decide_move(
            &move_op(&from, &to),
            &policies(Policy::Apply, Policy::Apply),
            &trash,
            &Quiet::default(),
        )
        .unwrap();
        assert!(!skip, "apply must produce a plain step {{}} (no skip)");
        assert!(!to.exists(), "no file should have been created at the target");
        std::fs::remove_dir_all(&tmp).ok();
    }

    // `skip` is available for a gone file too: it yields a `step {skip:true}`
    // (rewind), never touching the filesystem.
    #[test]
    fn skip_is_available_for_a_gone_file() {
        let tmp = scratch("skip");
        let trash = TrashDir::new(tmp.join("trash"));
        let op = move_op(&tmp.join("gone"), Path::new("/whatever"));
        let skip =
            decide_move(&op, &policies(Policy::Skip, Policy::Skip), &trash, &Quiet::default())
                .unwrap();
        assert!(skip, "skip must request the rewind even when the file is gone");
        std::fs::remove_dir_all(&tmp).ok();
    }

    // `ask` goes to the front end, and whatever it answers is applied.
    #[test]
    fn ask_is_the_front_end_s_decision() {
        struct Skipper;
        impl NavigationUi for Skipper {
            fn ask_move(&self, _: &str, _: &str, _: bool) -> Result<Policy, NavError> {
                Ok(Policy::Skip)
            }
            fn note(&self, _: &str) {}
        }
        let tmp = scratch("ask");
        let from = tmp.join("here.txt");
        std::fs::write(&from, b"x").unwrap();
        let trash = TrashDir::new(tmp.join("trash"));
        let op = move_op(&from, &tmp.join("there.txt"));
        let skip = decide_move(&op, &policies(Policy::Ask, Policy::Ask), &trash, &Skipper).unwrap();
        assert!(skip);
        assert!(from.exists(), "a skipped move moves nothing");
        std::fs::remove_dir_all(&tmp).ok();
    }

    // `apply` on a present file performs the `mv` and produces `step {}`.
    #[test]
    fn apply_on_a_present_file_moves_it() {
        let tmp = scratch("present");
        let (from, to) = (tmp.join("here.txt"), tmp.join("moved.txt"));
        std::fs::write(&from, b"x").unwrap();
        let trash = TrashDir::new(tmp.join("trash"));
        let skip = decide_move(
            &move_op(&from, &to),
            &policies(Policy::Apply, Policy::Apply),
            &trash,
            &Quiet::default(),
        )
        .unwrap();
        assert!(!skip);
        assert!(!from.exists() && to.exists(), "the file should have been moved");
        std::fs::remove_dir_all(&tmp).ok();
    }

    // `apply` when the destination is occupied trashes the occupant first, so
    // its content survives the overwrite (doc "Trash").
    #[test]
    fn apply_trashes_an_occupied_destination() {
        let tmp = scratch("occupied");
        let (from, to) = (tmp.join("here.txt"), tmp.join("victim.txt"));
        std::fs::write(&from, b"source").unwrap();
        std::fs::write(&to, b"victim-content").unwrap();
        let trash = TrashDir::new(tmp.join("trash"));
        let skip = decide_move(
            &move_op(&from, &to),
            &policies(Policy::Apply, Policy::Apply),
            &trash,
            &Quiet::default(),
        )
        .unwrap();
        assert!(!skip);
        assert_eq!(std::fs::read(&to).unwrap(), b"source", "the mv happened");
        // The victim's bytes are preserved in the trash, not destroyed.
        let entries = trash.entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].reason, Reason::Rollback);
        // The entry records the causing revision, but no metarecord — the op's
        // entity_uuid names the moved record, not this displaced occupant.
        assert_eq!(entries[0].metarecord, None);
        std::fs::remove_file(&to).unwrap();
        let blob = trash.restore(&entries[0].id).unwrap();
        assert_eq!(std::fs::read(blob).unwrap(), b"victim-content");
        std::fs::remove_dir_all(&tmp).ok();
    }

    // A directory at the destination cannot be trashed or overwritten: the step
    // is skipped (metadata rewinds, file stays) rather than aborting the whole
    // rollback.
    #[test]
    fn apply_skips_when_a_directory_occupies_the_destination() {
        let tmp = scratch("dirdest");
        let (from, to) = (tmp.join("here.txt"), tmp.join("blocking_dir"));
        std::fs::write(&from, b"x").unwrap();
        std::fs::create_dir_all(&to).unwrap();
        let trash = TrashDir::new(tmp.join("trash"));
        let skip = decide_move(
            &move_op(&from, &to),
            &policies(Policy::Apply, Policy::Apply),
            &trash,
            &Quiet::default(),
        )
        .unwrap();
        assert!(skip, "a directory destination must skip, not abort");
        assert!(from.exists() && to.is_dir(), "nothing was moved or trashed");
        assert!(trash.entries().unwrap().is_empty());
        std::fs::remove_dir_all(&tmp).ok();
    }

    // A *broken* symlink is a file like any other: it can be moved, and it can
    // be overwritten. `Path::exists()` follows the link, so it read the source
    // as gone (no `mv` — the link stayed behind) and the destination as free
    // (no trashing — the `mv` destroyed the link, against the trash's promise
    // that a rollback loses no byte).
    #[cfg(unix)]
    #[test]
    fn apply_moves_a_broken_symlink_and_trashes_a_broken_symlink_it_overwrites() {
        let tmp = scratch("brokenlink");
        let (from, to) = (tmp.join("moved_link"), tmp.join("occupied_link"));
        std::os::unix::fs::symlink("gone-target", &from).unwrap();
        std::os::unix::fs::symlink("other-gone-target", &to).unwrap();
        let trash = TrashDir::new(tmp.join("trash"));
        let skip = decide_move(
            &move_op(&from, &to),
            &policies(Policy::Apply, Policy::Apply),
            &trash,
            &Quiet::default(),
        )
        .unwrap();
        assert!(!skip, "the source is present, so the move applies");
        assert!(std::fs::symlink_metadata(&from).is_err(), "the source link was moved");
        assert_eq!(std::fs::read_link(&to).unwrap(), Path::new("gone-target"));
        assert_eq!(trash.entries().unwrap().len(), 1, "the overwritten link went to the trash");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// A daemon answering by the first `"METHOD /path"` substring that matches.
    struct Canned(Vec<(&'static str, Json)>);
    impl DaemonClient for Canned {
        fn request(&self, method: &str, path: &str, _: Option<&Json>) -> Result<Json, DaemonError> {
            let key = format!("{method} {path}");
            self.0
                .iter()
                .find(|(m, _)| key.contains(m))
                .map(|(_, body)| body.clone())
                .ok_or(DaemonError { status: Some(404), message: format!("no {key}") })
        }
    }

    fn tracked(uuid: &str, parent: &str, name: &str) -> Json {
        json!({"uuid": uuid, "version": 7, "fields": [{"name": "mfr_path", "value":
            {"type": "tree_ref", "value": {"parent": parent, "name": name}}}]})
    }

    /// A directory `A` (with `A/b.txt`) under the root, tracked.
    fn dir_repo<'a>(client: &'a Canned, root: &Path) -> Repo<'a> {
        Repo {
            client,
            repo: "r".into(),
            root: root.to_path_buf(),
            trash: TrashDir::new(root.join(".trash")),
        }
    }

    fn dir_daemon() -> Canned {
        Canned(vec![
            ("GET /repos/r/metarecords/dir/fields/mfr_path/resolve-tree", json!({"paths": ["/A"]})),
            (
                "GET /repos/r/metarecords/file/fields/mfr_path/resolve-tree",
                json!({"paths": ["/A/b.txt"]}),
            ),
            ("GET /repos/r/metarecords/dir", tracked("dir", "root", "A")),
            ("GET /repos/r/metarecords/file", tracked("file", "dir", "b.txt")),
            (
                "GET /repos/r/metarecords/root",
                json!({"uuid": "root", "fields": [{"name": "mfr_path",
                "value": {"type": "tree_ref", "value": {"parent": null, "name": ""}}}]}),
            ),
            (
                "POST /repos/r/query",
                json!({"results": [tracked("file", "dir", "b.txt")],
                "next_cursor": null}),
            ),
        ])
    }

    // A trashing redone sends the file back as a trashing does: captured, into
    // a new entry. A directory goes whole, and the steps for the files inside
    // it find them already covered.
    #[test]
    fn a_redone_trashing_sends_the_directory_back_once() {
        let root = scratch("retrash");
        std::fs::create_dir_all(root.join("A")).unwrap();
        std::fs::write(root.join("A/b.txt"), b"bee").unwrap();
        let daemon = dir_daemon();
        let repo = dir_repo(&daemon, &root);
        let mut covered = HashSet::new();
        let ui = Quiet::default();

        retrash(&repo, &json!({"id": 9, "entity_uuid": "dir"}), &mut covered, &ui).unwrap();
        retrash(&repo, &json!({"id": 10, "entity_uuid": "file"}), &mut covered, &ui).unwrap();

        assert!(!root.join("A").exists(), "the directory is back in the trash");
        let entries = repo.trash.entries().unwrap();
        assert_eq!(entries.len(), 1, "one entry, the directory whole");
        assert_eq!(entries[0].metarecord.as_deref(), Some("dir"));
        assert_eq!(entries[0].version, Some(7));
        let captured: Vec<&str> =
            entries[0].subtree.iter().filter(|n| n.trashed).map(|n| n.uuid.as_str()).collect();
        assert!(captured.contains(&"dir") && captured.contains(&"file"), "{captured:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    // No file on disk: nothing to send, and the user hears it.
    #[test]
    fn a_redone_trashing_without_its_file_only_says_so() {
        let root = scratch("retrash_gone");
        let daemon = dir_daemon();
        let repo = dir_repo(&daemon, &root);
        let ui = Quiet::default();
        retrash(&repo, &json!({"id": 9, "entity_uuid": "dir"}), &mut HashSet::new(), &ui).unwrap();
        assert!(repo.trash.entries().unwrap().is_empty());
        assert!(ui.notes.borrow().iter().any(|n| n.contains("not on disk")), "{:?}", ui.notes);
        std::fs::remove_dir_all(&root).ok();
    }

    fn manual_entry(id: &str, metarecord: Option<&str>) -> TrashEntry {
        TrashEntry {
            id: id.into(),
            original_path: "/x".into(),
            original_name: "x".into(),
            trashed_at: 0,
            size: 0,
            is_dir: false,
            reason: Reason::Manual,
            revision: None,
            metarecord: metarecord.map(str::to_owned),
            version: None,
            subtree: Vec::new(),
        }
    }

    #[test]
    fn trash_recovery_hint_matches_by_metarecord() {
        let entries = vec![
            manual_entry("aaa", Some("rec-1")),
            manual_entry("bbb", None),
            manual_entry("ccc", Some("rec-1")),
            manual_entry("ddd", Some("rec-2")),
        ];
        let hint = trash_recovery_hint(&entries, "rec-1").unwrap();
        assert!(hint.contains("aaa") && hint.contains("ccc") && hint.contains("mf trash restore"));
        assert!(trash_recovery_hint(&entries, "rec-3").is_none());
        assert!(trash_recovery_hint(&entries, "").is_none());
    }

    fn deleted_op(entity: &str, version_before: Option<u64>) -> Json {
        let mut op = json!({"op_type": "file_deleted", "entity_uuid": entity,
                            "filesystem": {"action": "restore_content"}});
        if let Some(v) = version_before {
            op["entity_version_before"] = json!(v);
        }
        op
    }

    // A file_deleted step whose content is in the trash at the matching version
    // is auto-restored (no skip); the daemon then applies the real inverse.
    #[test]
    fn deleted_step_auto_restores_the_matching_version() {
        let base = scratch("del");
        let trash = TrashDir::new(base.join("trash"));
        let file = base.join("doc.txt");
        std::fs::write(&file, b"content").unwrap();
        let e =
            trash.trash_path(&file, Reason::Manual, None, Some("rec-1".into()), Some(4)).unwrap();
        let mut entries = trash.entries().unwrap();
        let op = deleted_op("rec-1", Some(4));
        let skip = decide_deleted(
            &op,
            &trash,
            &mut entries,
            &mut HashSet::new(),
            &Quiet::default(),
            false,
        )
        .unwrap();
        assert!(!skip, "a matching trash entry must be auto-restored (step {{}})");
        assert_eq!(std::fs::read(&file).unwrap(), b"content", "the file is back");
        assert!(entries.is_empty(), "the consumed entry is removed");
        assert!(trash.entry(&e.id).is_err(), "the entry is gone");
        std::fs::remove_dir_all(&base).ok();
    }

    // A revision that orphans a record writes several fields (mfr_path and
    // mfr_path_old), so its operations restore to different versions while the
    // trash entry records only the version the record had *before* the whole
    // revision. Every op of the revision must reach the same decision.
    #[test]
    fn deleted_steps_of_one_revision_share_the_pre_revision_version() {
        let base = scratch("del3");
        let trash = TrashDir::new(base.join("trash"));
        let file = base.join("doc.txt");
        std::fs::write(&file, b"content").unwrap();
        trash.trash_path(&file, Reason::Manual, None, Some("rec-1".into()), Some(0)).unwrap();
        let mut entries = trash.entries().unwrap();
        let mut restored = HashSet::new();
        let ui = Quiet::default();

        let mut op = deleted_op("rec-1", Some(1));
        op["entity_version_before_revision"] = json!(0);
        let skip = decide_deleted(&op, &trash, &mut entries, &mut restored, &ui, false).unwrap();
        assert!(!skip, "the pre-revision version matches the trash entry: restore, don't skip");
        assert_eq!(std::fs::read(&file).unwrap(), b"content", "the file is back");
        assert!(entries.is_empty(), "the entry is consumed once");

        let mut op = deleted_op("rec-1", Some(0));
        op["entity_version_before_revision"] = json!(0);
        let skip = decide_deleted(&op, &trash, &mut entries, &mut restored, &ui, false).unwrap();
        assert!(!skip, "the record's content is restored: apply the inverse");
        std::fs::remove_dir_all(&base).ok();
    }

    // A version mismatch (or a forward step with no entity_version_before) does
    // not restore: the step is skipped and the file stays trashed.
    #[test]
    fn deleted_step_skips_on_version_mismatch() {
        let base = scratch("del2");
        let trash = TrashDir::new(base.join("trash"));
        let file = base.join("doc.txt");
        std::fs::write(&file, b"content").unwrap();
        trash.trash_path(&file, Reason::Manual, None, Some("rec-1".into()), Some(4)).unwrap();
        let mut entries = trash.entries().unwrap();
        let ui = Quiet::default();
        for version in [Some(9), None] {
            let op = deleted_op("rec-1", version);
            let skip =
                decide_deleted(&op, &trash, &mut entries, &mut HashSet::new(), &ui, false).unwrap();
            assert!(skip, "{version:?}");
            assert!(!file.exists(), "the file stays in the trash");
            assert_eq!(entries.len(), 1);
        }
        std::fs::remove_dir_all(&base).ok();
    }
}
