// Input-burst coalescing for async side effects. A held-down key repeats
// faster than a workspace.set round-trip (Tauri IPC + the panels reacting to
// the variable change), so naively awaiting the effect once per key event
// accumulates a backlog that keeps replaying after the key is released.
//
// `latestOnly(fn)` wraps an async function so that at most one run is in
// flight: the first call runs immediately, and every call made while it runs
// collapses into exactly one trailing re-run made with the latest arguments
// (intermediate calls are dropped — the effect is state-propagation-shaped,
// only the final state matters). Collapsed callers get the trailing run's
// promise, so awaiting still means "the effect has caught up with my call".

export function latestOnly(fn) {
  let inFlight = null; // promise of the running fn, null when idle
  let trailing = null; // promise of the single queued re-run, null when none
  let trailingArgs = null; // latest-wins arguments for the trailing re-run

  return function call(...args) {
    if (inFlight === null) {
      // fn starts synchronously (the first key event propagates right away);
      // a synchronous throw is surfaced as a rejection to stay call-shaped.
      try {
        inFlight = Promise.resolve(fn(...args)).finally(() => {
          inFlight = null;
        });
      } catch (error) {
        return Promise.reject(error);
      }
      return inFlight;
    }
    trailingArgs = args;
    if (trailing === null) {
      // Sequence after the in-flight run whether it resolves or rejects; its
      // outcome belongs to its own callers, not to the trailing one.
      trailing = inFlight
        .catch(() => {})
        .then(() => {
          trailing = null;
          const latest = trailingArgs;
          trailingArgs = null;
          return call(...latest);
        });
    }
    return trailing;
  };
}
