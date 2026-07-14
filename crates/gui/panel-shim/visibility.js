// Visibility gate, served at /__visibility.js and used by the shim to
// back metafolder.visible / metafolder.whenVisible. Panel construction
// (command registration, listeners) happens at iframe load; expensive
// work — the first daemon fetch, a directory listing — is deferred
// through the gate to the first actual display, so panel types can be
// pre-instantiated hidden at startup for the cost of registration only.

export function createVisibilityGate() {
  let visible = false;
  // A Set: re-arming with the same (stable) callback while hidden must
  // not run it twice on the first show.
  /** @type {Set<() => void>} */
  const pending = new Set();

  return {
    get visible() {
      return visible;
    },

    /** Records a visibility change; entering visibility flushes the
     *  pending callbacks (each fires once).
     *  @param {boolean} next */
    set(next) {
      visible = next;
      if (!visible) return;
      const ready = [...pending];
      pending.clear();
      for (const fn of ready) fn();
    },

    /** Runs `fn` now when visible, otherwise once on the next show.
     *  @param {() => void} fn */
    whenVisible(fn) {
      if (visible) fn();
      else pending.add(fn);
    },
  };
}
