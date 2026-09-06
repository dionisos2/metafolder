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
