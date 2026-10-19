// A panel instance's run scope (doc "Command transactions"). A panel's
// command handler works through the `metafolder` object the panel was mounted
// with, and JavaScript has no context that follows an `await`, so a call cannot
// say which run it belongs to. The instance can: while one of its commands
// runs, whatever it does through its API — the handler, the functions it calls,
// a listener or a timer firing meanwhile — is in that run's transaction. Runs
// at once on one instance share one transaction, ended after the last.

type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

export interface RunScope {
  /** The transaction the instance is in now, if any. */
  txn(): number | undefined;
  /** Runs `fn` in the instance's transaction, beginning one if none is open. */
  run<T>(fn: () => T | Promise<T>): Promise<T>;
}

export function createRunScope(invoke: Invoke): RunScope {
  /** The open transaction and how many runs are in it. `begun` is shared, so
   *  a run starting while the begin is in flight joins rather than begins. */
  let open: { begun: Promise<number | undefined>; id: number | undefined; runs: number } | null =
    null;

  return {
    txn: () => open?.id,
    async run(fn) {
      if (open === null) {
        const begun = invoke('txn_begin').then((id) => (typeof id === 'number' ? id : undefined));
        open = { begun, id: undefined, runs: 0 };
      }
      const scope = open;
      scope.runs += 1;
      try {
        scope.id = await scope.begun;
        return await fn();
      } finally {
        scope.runs -= 1;
        if (scope.runs === 0) {
          if (open === scope) open = null;
          if (scope.id !== undefined) await invoke('txn_end', { txn: scope.id });
        }
      }
    },
  };
}
