// A command resolves when its effect is visible (doc "When a command has
// finished"): what it set off — the layout events Rust emitted, the panels it
// shows mounting, the loads their visibility starts, the loads those loads'
// results start in turn — has ended. `settle` is that wait, over seams the
// shell provides.

import { describe, expect, test } from 'vitest';
import { settle, type SettleDeps } from '../src/lib/settle';
import { createWorkingTracker } from '../src/lib/working';

/** A promise plus the handle to resolve it from the test. */
function deferred<T = void>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((res) => {
    resolve = res;
  });
  return { promise, resolve };
}

/** Seams over a real tracker; each frame runs the callbacks queued for it,
 *  like `requestAnimationFrame`. */
function harness() {
  const tracker = createWorkingTracker(300);
  const onFrame: (() => void)[] = [];
  const onBarrier: (() => void)[] = [];
  const log: string[] = [];
  let mounts: Promise<void> = Promise.resolve();
  const deps: SettleDeps = {
    barrier: async () => {
      log.push('barrier');
      for (const fn of onBarrier.splice(0)) fn();
    },
    frame: async () => {
      // A frame is a task of its own, as in the browser.
      await new Promise((r) => setTimeout(r, 0));
      log.push('frame');
      for (const fn of onFrame.splice(0)) fn();
    },
    mounts: () => mounts,
    busy: () => tracker.pending(),
    idle: () => tracker.idle(),
  };
  return {
    tracker,
    deps,
    log,
    onFrame,
    onBarrier,
    setMounts: (p: Promise<void>) => {
      mounts = p;
    },
  };
}

describe('settle', () => {
  test('with nothing set off, it returns after the barrier and a frame', async () => {
    const h = harness();
    await settle(h.deps);
    expect(h.log[0]).toBe('barrier');
    expect(h.log).toContain('frame');
  });

  test('waits for a load started by an event applied at the barrier', async () => {
    const h = harness();
    const load = deferred();
    let ended = false;
    h.onBarrier.push(() => void h.tracker.track(load.promise, 'POST /query'));
    const settled = settle(h.deps).then(() => (ended = true));
    await new Promise((r) => setTimeout(r, 0));
    expect(ended).toBe(false);
    load.resolve();
    await settled;
    expect(ended).toBe(true);
  });

  test('waits for a load a frame starts (a panel shown, then told it is visible)', async () => {
    const h = harness();
    const load = deferred();
    let ended = false;
    h.onFrame.push(() => void h.tracker.track(load.promise, 'POST /query'));
    const settled = settle(h.deps).then(() => (ended = true));
    await new Promise((r) => setTimeout(r, 0));
    expect(ended).toBe(false);
    load.resolve();
    await settled;
  });

  test('waits for a load the end of another starts', async () => {
    const h = harness();
    const first = deferred();
    const second = deferred();
    let ended = false;
    h.onBarrier.push(() => {
      void h.tracker.track(first.promise, 'GET /a').then(() => {
        // The answer's handler queues its follow-up for the next frame.
        h.onFrame.push(() => void h.tracker.track(second.promise, 'GET /b'));
      });
    });
    const settled = settle(h.deps).then(() => (ended = true));
    first.resolve();
    await new Promise((r) => setTimeout(r, 0));
    expect(ended).toBe(false);
    second.resolve();
    await settled;
    expect(h.tracker.pending()).toBe(0);
  });

  test('waits for the visible panels to be mounted', async () => {
    const h = harness();
    const mount = deferred();
    h.setMounts(mount.promise);
    let ended = false;
    const settled = settle(h.deps).then(() => (ended = true));
    await new Promise((r) => setTimeout(r, 0));
    expect(ended).toBe(false);
    mount.resolve();
    await settled;
  });

  test('a panel that never stops loading does not hold the command forever', async () => {
    const h = harness();
    // Every frame starts a load that ends at once: never quiet two frames running.
    const restart = () => {
      void h.tracker.track(Promise.resolve(), 'poll');
      h.onFrame.push(restart);
    };
    h.onFrame.push(restart);
    await settle(h.deps, 5);
    expect(h.log.filter((e) => e === 'frame').length).toBeLessThanOrEqual(12);
  });
});
