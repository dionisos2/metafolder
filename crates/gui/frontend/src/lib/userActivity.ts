// The user's activity, reported to the daemon (doc "Folding in idle time"):
// what writes to many records left pending is folded once a repository has
// been quiet for a while, so the daemon is told when the user is at work.

import { invoke } from './ipc';
import { visibleWorkspaces, workspaceById } from './store.svelte';

/** At most one report this often: far finer than the daemon's thirty seconds. */
export const ACTIVITY_EVERY_MS = 2000;

/**
 * A function to call on each user action: it sends `repos()` to `send`, at most
 * once per interval. Nothing is sent, and nothing counted, while no repository
 * is on screen.
 */
export function createActivityReporter(
  send: (repos: string[]) => void,
  repos: () => string[],
  now: () => number = Date.now,
): () => void {
  let last = -Infinity;
  return () => {
    const t = now();
    if (t - last < ACTIVITY_EVERY_MS) return;
    const on = repos();
    if (on.length === 0) return;
    last = t;
    send(on);
  };
}

/** The repositories of the workspaces on screen. */
function visibleRepos(): string[] {
  const out = new Set<string>();
  for (const ws of visibleWorkspaces()) {
    const repo = workspaceById(ws)?.active_repo;
    if (repo) out.add(repo);
  }
  return [...out];
}

/** Reports the user's keys, clicks and scrolling on `target`. */
export function installUserActivity(target: Window): void {
  const note = createActivityReporter(
    (repos) => {
      for (const repo of repos) {
        // A failure is not the user's business: the daemon folds a little early.
        void invoke('daemon_request', {
          method: 'POST',
          path: `/repos/${repo}/activity`,
          body: null,
        }).catch(() => {});
      }
    },
    visibleRepos,
  );
  for (const type of ['keydown', 'pointerdown', 'wheel']) {
    target.addEventListener(type, note, { capture: true, passive: true });
  }
}
