// When a command has finished (doc "When a command has finished"): not when
// its handler returns, but when what the handler set off has ended — the
// events Rust emitted for it applied, the panels it shows mounted and told they
// are visible, the daemon reads that starts, and the reads *their* answers
// start. Only then does `dispatch` resolve, so the next command of a sequence
// (a macro, a user command, a script's `mf gui command`) acts on what the user
// would see, never on the rows of a list still loading.
//
// No command declares what it sets off: the wait is the same for all of them,
// over what the shell can observe — every foreground daemon call goes through
// `daemonWork`, and every panel instance has its `mounted` promise.

import { invoke, listen } from './ipc';
import { daemonWork } from './working';

export interface SettleDeps {
  /** Resolves once every event Rust emitted before the call is applied. */
  barrier(): Promise<void>;
  /** One rendering frame, then the zero-delay timers queued during it. */
  frame(): Promise<void>;
  /** Resolves once the panels on screen are mounted. */
  mounts(): Promise<void>;
  /** The foreground daemon calls in flight. */
  busy(): number;
  /** Resolves once none is in flight. */
  idle(): Promise<void>;
}

/** Frames in a row with nothing in flight that count as quiet: a load is
 *  often started a timer *after* the frame that shows its panel. */
const QUIET_FRAMES = 2;

/**
 * Waits until what a command set off has ended, or `maxRounds` rounds have
 * passed — a panel that reloads without end (a poll) must not hold every
 * command forever; it is then as settled as it will get.
 */
export async function settle(deps: SettleDeps, maxRounds = 20): Promise<void> {
  await deps.barrier();
  let quiet = 0;
  for (let round = 0; round < maxRounds; round++) {
    await deps.frame();
    await deps.mounts();
    if (deps.busy() === 0) {
      quiet += 1;
      if (quiet >= QUIET_FRAMES) return;
      continue;
    }
    quiet = 0;
    await deps.idle();
  }
}

// ── The shell's seams ──────────────────────────────────────────────────────

/** A frame that also comes when the window is not drawn (minimised, hidden):
 *  WebKit then stops `requestAnimationFrame`, and a command run by a script
 *  must still resolve. */
const FRAME_FALLBACK_MS = 50;

function frame(): Promise<void> {
  return new Promise((resolve) => {
    let done = false;
    const next = () => {
      if (done) return;
      done = true;
      setTimeout(resolve, 0);
    };
    requestAnimationFrame(next);
    setTimeout(next, FRAME_FALLBACK_MS);
  });
}

let mountsProvider: (() => Promise<void>) | null = null;

/** Installed by the panel host: the visible instances' mounts. */
export function setMountsProvider(provider: (() => Promise<void>) | null): void {
  mountsProvider = provider;
}

// Barriers in flight, by id. Rust emits `event-barrier` after every event it
// emitted before (one emitter, in order), so its arrival means they are applied.
const barriers = new Map<number, () => void>();
let nextBarrier = 0;
let listening: Promise<unknown> | null = null;
const BARRIER_TIMEOUT_MS = 2000;

async function barrier(): Promise<void> {
  listening ??= listen<{ id: number }>('event-barrier', (event) => {
    barriers.get(event.payload.id)?.();
    barriers.delete(event.payload.id);
  });
  await listening;
  const id = nextBarrier++;
  const arrived = new Promise<void>((resolve) => barriers.set(id, resolve));
  await invoke('event_barrier', { id });
  // An event that never comes (a dead listener) must not hang every command.
  await Promise.race([arrived, new Promise((resolve) => setTimeout(resolve, BARRIER_TIMEOUT_MS))]);
  barriers.delete(id);
}

/** The settle the shell's `dispatch` waits for. */
export function settleShell(): Promise<void> {
  return settle({
    barrier,
    frame,
    mounts: () => mountsProvider?.() ?? Promise.resolve(),
    busy: () => daemonWork.pending(),
    idle: () => daemonWork.idle(),
  });
}
