// The "the daemon is working" indicator (spec-gui "Working vs. awaiting an
// answer").
//
// A daemon call is normally too fast to be worth showing — announcing every one
// of them would strobe. A few are not: a write the daemon has to settle, a query
// over a large repository. Those hold the repository's connection for their
// whole duration, so every other panel action queues behind them, and with
// nothing on screen the GUI reads as having stopped responding rather than as
// busy. So a call is announced only once it has outlasted a short grace period.
//
// Only *foreground* calls are tracked — what a panel or a command does because
// the user asked. The background polls (the change feed, the task list) have
// nobody waiting on them and must stay silent.

/** What the indicator shows: the oldest call still running, and how many. */
export interface WorkingState {
  label: string;
  count: number;
}

export interface WorkingTracker {
  /** Follows `call` for as long as it runs, and hands it back unchanged. */
  track<T>(call: Promise<T>, label: string): Promise<T>;
  /** The current announcement, or null while there is nothing to show. */
  state(): WorkingState | null;
  /** Calls `listener` on every change; returns the unsubscribe function. */
  subscribe(listener: (state: WorkingState | null) => void): () => void;
}

/** The grace period a call must outlast before it is announced, in ms. */
export const WORKING_GRACE_MS = 300;

export function createWorkingTracker(graceMs = WORKING_GRACE_MS): WorkingTracker {
  // Insertion-ordered, so the first entry is the call that has been running
  // longest — the one worth naming.
  const running = new Map<number, string>();
  const listeners = new Set<(state: WorkingState | null) => void>();
  let nextId = 0;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let announced = false;

  function current(): WorkingState | null {
    if (!announced) return null;
    const [label] = running.values();
    return label === undefined ? null : { label, count: running.size };
  }

  let last: WorkingState | null = null;
  function notify() {
    const now = current();
    // Only real changes: the count moving while nothing is announced is not one.
    if (now?.label === last?.label && now?.count === last?.count) return;
    last = now;
    for (const listener of listeners) listener(now);
  }

  return {
    track<T>(call: Promise<T>, label: string): Promise<T> {
      const id = nextId++;
      running.set(id, label);
      if (timer === null && !announced) {
        timer = setTimeout(() => {
          timer = null;
          announced = running.size > 0;
          notify();
        }, graceMs);
      }
      return call.finally(() => {
        running.delete(id);
        if (running.size === 0) {
          if (timer !== null) clearTimeout(timer);
          timer = null;
          announced = false;
        }
        notify();
      });
    },
    state: current,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

/** The shell's single tracker, shared by the panel API and the command layer. */
export const daemonWork = createWorkingTracker();

// ── Daemon tasks ─────────────────────────────────────────────────────────────
//
// The same rule, for the task bar. The bar is a row of the shell's column
// layout, so showing one takes height away from the panels: every panel
// re-lays out, mid-scroll and mid-read, for something that will be gone by the
// next poll. A watcher flush is the ordinary case — it lasts milliseconds and
// fires whenever anything at all touches a watched file — so a task earns its
// place only once it has been there long enough to be worth the rearrangement.
// A long one (a reconcile, a directory of a hundred thousand files arriving)
// crosses the grace immediately and is shown as before.

/** How long a daemon task must have been in flight before the bar shows it. */
export const TASK_GRACE_MS = 1000;

/**
 * The tasks old enough to show, in the order the daemon listed them.
 *
 * `seen` carries the first sighting of each task id across calls and is pruned
 * of whatever the daemon no longer lists — the caller owns it and keeps it for
 * as long as the bar is mounted. Timing from the first *sighting* rather than
 * from the task's own `started_at` is deliberate: that field is second-grained,
 * which is coarser than the grace itself.
 */
export function settledTasks<T extends { id: string }>(
  seen: Map<string, number>,
  tasks: T[],
  now: number,
  graceMs = TASK_GRACE_MS,
): T[] {
  const live = new Set(tasks.map((t) => t.id));
  for (const id of seen.keys()) {
    if (!live.has(id)) seen.delete(id);
  }
  return tasks.filter((t) => {
    const first = seen.get(t.id);
    if (first === undefined) {
      seen.set(t.id, now);
      return false;
    }
    return now - first >= graceMs;
  });
}
