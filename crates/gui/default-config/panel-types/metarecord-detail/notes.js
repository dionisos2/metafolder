// The lines under the detail panel's head: the record's file is gone (purple),
// its volume is unplugged (amber), whether the watcher would record a change
// at it, and how busy it has been (doc "Checking whether a path is watched",
// doc "Watch activity", doc "Unmounted volumes in the GUI"). Each is fetched
// once per load and dropped when the panel has moved on to another record
// meanwhile. What it needs from the panel arrives through `ctx`:
//   shown()       -> the loaded metarecord, or null
//   exists(path)  -> whether a path is on disk (orphan check)
//   render()      -> repaint (the reconcile button follows the watch verdict)

import { byId } from '/__ui.js';
import { orphanState, orphanLabel } from '/__orphan.js';
import { fetchMounts, offlineMountFor, relativeTo, unavailableLabel } from '/__mounts.js';
import { fetchWatched, summarizeWatched } from '/__watched.js';
import { activityTitle, fetchActivity, isHot, operationsTitle } from '/__activity.js';

/**
 * @param {{
 *   root: ShadowRoot,
 *   daemon: MetafolderApi['daemon'],
 *   exists: (path: string) => Promise<boolean>,
 *   shown: () => Metafolder.Metarecord|null,
 *   render: () => void,
 * }} ctx
 */
export function createNotes({ root, daemon, exists, shown: shownNow, render }) {
  const orphanNote = byId(root, 'orphan-note');
  const mountNote = byId(root, 'mount-note');
  const watchNote = byId(root, 'watch-note');
  const activityNote = byId(root, 'activity-note');

  /** @type {{shown: Metafolder.Metarecord|null, promise: Promise<string[]>}} */
  let pathsMemo = { shown: null, promise: Promise.resolve([]) };

  /** The loaded record's absolute file path(s), resolved once per load — the
   *  file actions, the unmounted-volume note and the orphan note all want
   *  them, and each used to ask the daemon for itself. Keyed on the loaded
   *  object, so a `metarecords:dirty` reload (a rename, say) re-resolves.
   *  @param {string} repo @param {Metafolder.Metarecord} shown */
  function recordPaths(repo, shown) {
    if (pathsMemo.shown !== shown) {
      pathsMemo = { shown, promise: daemon.metarecordPaths(repo, shown).catch(() => []) };
    }
    return pathsMemo.promise;
  }

  /**
   * Shows the purple orphan line when the tracked file is gone — or, first, the
   * amber "volume not mounted" line when it is merely unavailable: a stale path
   * on an unplugged drive is not an orphan, and saying so would invite the user
   * to clean up records they want kept (doc "Unmounted volumes in the GUI").
   * @param {string} repo
   */
  async function fillOrphanNote(repo) {
    const shown = shownNow();
    if (!shown) return;
    const mount = await unavailableMount(repo, shown).catch(() => null);
    if (shownNow() !== shown) return;
    if (mount) {
      mountNote.textContent = unavailableLabel(mount);
      mountNote.hidden = false;
      return;
    }
    const state = await orphanState(shown, {
      metarecordPaths: (m) => recordPaths(repo, m),
      exists,
    }).catch(() => null);
    if (state === null || shownNow() !== shown) return;
    orphanNote.textContent = orphanLabel(state);
    orphanNote.hidden = false;
  }

  /**
   * The offline mount point the record's file sits on, or null. Every path of
   * the record must be behind one: a record reachable at another, live location
   * is not unavailable.
   *
   * @param {string} repo @param {Metafolder.Metarecord} shown
   */
  async function unavailableMount(repo, shown) {
    const mounts = await fetchMounts(daemon, repo);
    if (mounts.length === 0) return null;
    const [rootDir, paths] = await Promise.all([
      daemon.repoRoot(repo),
      recordPaths(repo, shown),
    ]);
    if (paths.length === 0) return null;
    const found = paths.map((abs) => offlineMountFor(mounts, relativeTo(rootDir, abs)));
    return found.every(Boolean) ? found[0] : null;
  }

  /** The fetched watch state of the displayed record. Keyed on the loaded
   *  object (a `metarecords:dirty` reload re-resolves), like pathsMemo.
   *  `info` is null when there is nothing to judge — no paths, or the daemon
   *  did not answer — and the note then says nothing. */
  /** @type {{shown: Metafolder.Metarecord|null, info: {watched: boolean, title: string}|null}} */
  let watchMemo = { shown: null, info: null };

  /** Fills the watch note: the watcher's own answer for the displayed record's
   *  paths — would a change at the file be recorded? Shown for both states
   *  (the point is seeing which one it is); a record with no paths (nothing
   *  to watch) and a failed fetch show nothing. Fires once per load, guarded
   *  like the other fills: a selection that moved on must not have its note
   *  overwritten by the previous record's answer.
   *  @param {string} repo */
  async function fillWatchNote(repo) {
    const shown = shownNow();
    if (!shown) return;
    if (watchMemo.shown === shown) return; // already answered for this record
    // The record's own repo-root-relative path (the same resolution the list
    // panel marks with), as the list the endpoint answers. An unanswered read says nothing about the watch — leave
    // the note hidden and decide on a later load, like the orphan note does.
    const byUuid = await daemon.treePaths(repo, 'mfr_path', [shown.uuid]).catch(() => null);
    const rels = byUuid?.[shown.uuid];
    let info = null;
    /** @type {import('/__activity.js').Activity|null} */
    let activity = null;
    if (rels && rels.length > 0) {
      const byPath = await fetchWatched(daemon, repo, rels);
      info = summarizeWatched(rels.map((rel) => byPath.get(rel)));
      activity = await fetchActivity(daemon, repo, rels);
    }
    if (shownNow() !== shown) return;
    watchMemo = { shown, info };
    applyWatchNote(info);
    applyActivityNote(activity, rels ?? []);
    // The reconcile button's label follows the fetched answer (needsWatch):
    // "Watch and reconcile" only when the record is genuinely not watched.
    render();
  }

  /** @param {string} text @param {boolean} watched */
  function showWatchNote(text, watched) {
    watchNote.textContent = text;
    watchNote.classList.toggle('watched', watched);
    watchNote.classList.toggle('unwatched', !watched);
    watchNote.hidden = false;
  }

  /** @param {{watched: boolean, title: string}|null} info */
  function applyWatchNote(info) {
    if (info) showWatchNote(info.title, info.watched);
    else watchNote.hidden = true;
  }

  /** The watch activity note (doc "Watch activity"): how many watcher
   *  events arrived at the record's file (under it, for a directory) since the
   *  load. Were there several paths (an older repository's multi-position
   *  record), the busiest one is shown — the counts of nested paths overlap,
   *  so a sum would count events twice. What the flushes wrote to the log for
   *  them follows, when they wrote anything. Nothing is shown
   *  for a quiet record or an unanswered call.
   *  @param {import('/__activity.js').Activity|null} activity @param {string[]} rels */
  function applyActivityNote(activity, rels) {
    const events = Math.max(0, ...rels.map((rel) => activity?.counts.get(rel) ?? 0));
    if (!activity || events === 0) {
      activityNote.hidden = true;
      return;
    }
    const operations = Math.max(0, ...rels.map((rel) => activity.operations.get(rel) ?? 0));
    activityNote.textContent =
      activityTitle(events, activity.total, activity.sinceMs) +
      (operations > 0
        ? ` · ${operationsTitle(operations, activity.totalOperations ?? 0, activity.sinceMs)}`
        : '');
    activityNote.classList.toggle('hot', isHot(events, activity.total));
    activityNote.hidden = false;
  }

  return {
    recordPaths,

    /** Hides every note (a load begins, or nothing is shown). */
    hide() {
      orphanNote.hidden = true;
      mountNote.hidden = true;
      watchNote.hidden = true;
      activityNote.hidden = true;
    },

    /** Fetches and shows the notes of the record shown now, in `repo`.
     *  @param {string} repo */
    async fill(repo) {
      await Promise.all([fillOrphanNote(repo), fillWatchNote(repo)]);
    },

    /** Whether the shown record is not watched — what the reconcile button
     *  offers to change. */
    needsWatch() {
      const shown = shownNow();
      if (!shown) return false;
      // The watcher's own answer when it has landed: it knows about inheritance,
      // the ignore patterns, the exclusions and the budget — the raw field only
      // says what THIS record carries, so a record inheriting `mf_watch = true`
      // would otherwise read as unwatched.
      if (watchMemo.shown === shown && watchMemo.info) return !watchMemo.info.watched;
      const watch = (shown.fields ?? []).find((f) => f.name === 'mf_watch');
      return !watch || watch.value.type !== 'bool' || watch.value.value !== true;
    },
  };
}
