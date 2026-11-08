// The life of one panel instance (PanelHost.svelte): from its mount, which
// awaits the panel's files, to its teardown, which may come first — its
// workspace closed while the files were on their way. A mount that ends after
// the teardown is cleaned up at once: otherwise the panel would run on in a
// detached shadow root, subscribed to the change feed, with nobody left to
// call its cleanup.

export interface PanelLifecycle {
  /** False once torn down: a mount still in flight should stop there. */
  alive(): boolean;
  /** What the panel's `mount` returned: its cleanup, when a function. */
  mounted(returned: unknown): void;
  /** The teardown: runs the cleanup once, now or when the mount ends. */
  dispose(): void;
}

function runQuietly(cleanup: (() => void) | null): void {
  try {
    cleanup?.();
  } catch {
    /* a panel's cleanup must not block teardown */
  }
}

export function createPanelLifecycle(): PanelLifecycle {
  let cleanup: (() => void) | null = null;
  let disposed = false;
  return {
    alive: () => !disposed,
    mounted(returned) {
      const fn = typeof returned === 'function' ? (returned as () => void) : null;
      if (disposed) runQuietly(fn);
      else cleanup = fn;
    },
    dispose() {
      if (disposed) return;
      disposed = true;
      runQuietly(cleanup);
      cleanup = null;
    },
  };
}
