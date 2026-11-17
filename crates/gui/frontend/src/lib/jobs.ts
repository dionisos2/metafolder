// The shell's own long commands, shown in the task bar (doc "Status bar and
// task bar").
//
// A builtin that loops over many quick calls — trashing a hundred duplicate
// copies one by one — is invisible to the other two indicators: no call lasts
// long enough to be announced (lib/working), and it is no daemon task. Yet the
// run as a whole can take minutes, and with nothing on screen it reads as a
// command that did nothing. Such a command registers a job for its whole run
// and reports its counts, the way a script does with `mf gui progress`.

import { WORKING_GRACE_MS } from "./working";

/** One running job, as the task bar draws it. `done`/`total` give a bar when
 *  both are known, `phase` names the current step. */
export interface Job {
  id: number;
  /** The workspace it runs for: shown while that workspace is on screen. */
  workspace: string;
  label: string;
  phase: string | null;
  done: number | null;
  total: number | null;
  /** When it started (ms since the epoch), for the grace before it shows. */
  started: number;
}

export type JobProgress = Partial<Pick<Job, "phase" | "done" | "total">>;

export interface JobHandle {
  id: number;
  /** Changes what is named; ignored once the job has ended. */
  update(progress: JobProgress): void;
  /** Removes the entry; a second call does nothing. */
  end(): void;
}

export interface Jobs {
  start(workspace: string, label: string): JobHandle;
  /** Runs `work` as one job, ended however `work` returns or throws. */
  run<T>(workspace: string, label: string, work: (job: JobHandle) => Promise<T>): Promise<T>;
  list(): Job[];
  /** Calls `listener` with the list on every change; returns the unsubscribe. */
  subscribe(listener: (jobs: Job[]) => void): () => void;
}

export function createJobs(): Jobs {
  const running = new Map<number, Job>();
  const listeners = new Set<(jobs: Job[]) => void>();
  let nextId = 0;

  const list = () => [...running.values()].map((job) => ({ ...job }));
  const notify = () => {
    const now = list();
    for (const listener of listeners) listener(now);
  };

  function start(workspace: string, label: string): JobHandle {
    const id = nextId++;
    running.set(id, {
      id,
      workspace,
      label,
      phase: null,
      done: null,
      total: null,
      started: Date.now(),
    });
    notify();
    return {
      id,
      update(progress) {
        const job = running.get(id);
        if (!job) return;
        Object.assign(job, progress);
        notify();
      },
      end() {
        if (running.delete(id)) notify();
      },
    };
  }

  return {
    start,
    async run(workspace, label, work) {
      const job = start(workspace, label);
      try {
        return await work(job);
      } finally {
        job.end();
      }
    },
    list,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

/** The shell's single registry, which the task bar shows. */
export const jobs = createJobs();

/** The jobs the task bar shows at `now`: those of the workspaces on screen that
 *  have run for the grace of a slow daemon call — a run over in a moment must
 *  not flash an entry (doc "Status bar and task bar"). Pure, so it is
 *  unit-tested. */
export function shownJobs(list: Job[], visible: string[], now: number): Job[] {
  return list.filter(
    (job) => visible.includes(job.workspace) && now - job.started >= WORKING_GRACE_MS,
  );
}
