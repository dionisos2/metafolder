// The checked multi-selection (/__multi-select.js): one workspace-wide set of
// metarecords — the `selected_metarecords` variable, the target of the bulk
// operations and of `mf gui selected` — gathered across several lists and from
// the file manager (spec-gui "Workspace variables", "The checked selection").
// What this pins is the mirror discipline: every mutation is a
// read-modify-write of the variable whose echo comes back to the writing panel
// too (the backend broadcasts `workspace-var-changed` to every instance of the
// workspace), so an echo of our own write must never regress a later state,
// while a genuinely external change must be adopted — and a metarecord that no
// longer exists is pruned, or it stays checked but unshowable and ununcheckable.

import { describe, expect, test, vi } from 'vitest';
import { createMultiSelect } from '../../panel-shim/multi-select.js';

type Listener = (value: unknown, key?: string) => void;

/** A workspace var store echoing every write back to its listeners, like the
 *  Rust backend does — but only when the test says the event arrived, so the
 *  "old echo lands after a newer write" ordering can be staged. */
function stubWorkspace(vars: Record<string, unknown> = {}) {
  const store = new Map<string, unknown>(Object.entries(vars));
  const listeners = new Map<string, Set<Listener>>();
  /** Every `set` payload, in order — what the panel published. */
  const writes: { key: string; value: unknown }[] = [];
  /** Echoes the backend event loop has not delivered yet. */
  const pendingEchoes: { key: string; value: unknown }[] = [];
  const notify = (key: string, value: unknown) => {
    for (const l of listeners.get(key) ?? []) l(value);
    for (const l of listeners.get('*') ?? []) l(value, key);
  };
  return {
    api: {
      get: async (key: string) => store.get(key) ?? null,
      set: async (key: string, value: unknown) => {
        store.set(key, value);
        writes.push({ key, value });
        pendingEchoes.push({ key, value });
      },
      adoptRepo: async () => {},
      onChange: (key: string, listener: Listener) => {
        let set = listeners.get(key);
        if (!set) {
          set = new Set();
          listeners.set(key, set);
        }
        set.add(listener);
      },
    },
    store,
    writes,
    writesOf: (key: string) => writes.filter((w) => w.key === key).map((w) => w.value),
    /** Deliver the next queued echo (the writer hears its own event too). */
    deliverEcho() {
      const echo = pendingEchoes.shift();
      if (echo) notify(echo.key, echo.value);
    },
    deliverAll() {
      while (pendingEchoes.length > 0) this.deliverEcho();
    },
    /** A change from another panel or a script: stored and announced at once. */
    externalSet(key: string, value: unknown) {
      store.set(key, value);
      notify(key, value);
    },
  };
}

/** A daemon whose `uuid_in` query answers with the metarecords that still
 *  exist (their uuids, the bare `select` the pruning query omits). */
function stubDaemon(existing: Set<string>) {
  return {
    call: vi.fn(async (_method: string, _path: string, body: unknown) => {
      const query = (body as { query?: { type?: string; uuids?: string[] } })?.query;
      if (query?.type === 'uuid_in') {
        return { results: (query.uuids ?? []).filter((u) => existing.has(u)) };
      }
      return { results: [] };
    }),
  };
}

describe('createMultiSelect', () => {
  test('toggle checks then unchecks one metarecord', async () => {
    const ws = stubWorkspace();
    const selection = createMultiSelect({ workspace: ws.api, daemon: stubDaemon(new Set()) });
    await selection.load();
    await selection.toggle('a');
    expect(selection.values()).toEqual(['a']);
    expect(ws.writesOf('selected_metarecords')).toEqual([['a']]);
    await selection.toggle('a');
    expect(selection.values()).toEqual([]);
    expect(ws.writesOf('selected_metarecords')).toEqual([['a'], []]);
  });

  test('an echo of our own write never regresses a later state', async () => {
    const ws = stubWorkspace();
    const selection = createMultiSelect({ workspace: ws.api, daemon: stubDaemon(new Set()) });
    await selection.load();
    await selection.toggle('a');
    await selection.toggle('b'); // written while the first echo is still in flight
    // The first write's echo arrives late, carrying the older ['a'] state.
    ws.deliverEcho();
    expect(selection.values()).toEqual(['a', 'b']);
    ws.deliverEcho();
    expect(selection.values()).toEqual(['a', 'b']);
  });

  test('an external change is adopted and re-rendered', async () => {
    const ws = stubWorkspace();
    const render = vi.fn();
    const selection = createMultiSelect({
      workspace: ws.api,
      daemon: stubDaemon(new Set()),
      render,
    });
    await selection.load();
    render.mockClear();
    ws.externalSet('selected_metarecords', ['x']);
    expect(selection.values()).toEqual(['x']);
    expect(selection.has('x')).toBe(true);
    expect(render).toHaveBeenCalled();
  });

  test('add unions onto what is already checked, keeping the order', async () => {
    const ws = stubWorkspace({ selected_metarecords: ['x'] });
    const selection = createMultiSelect({ workspace: ws.api, daemon: stubDaemon(new Set()) });
    await selection.load();
    await selection.add(['a', 'x', 'b']);
    expect(selection.values()).toEqual(['x', 'a', 'b']);
    expect(ws.writesOf('selected_metarecords')).toEqual([['x', 'a', 'b']]);
    await selection.add(['b']); // already checked: no write at all
    expect(ws.writesOf('selected_metarecords')).toHaveLength(1);
  });

  test('clear empties the selection and says so', async () => {
    const ws = stubWorkspace({ selected_metarecords: ['x', 'y'] });
    const selection = createMultiSelect({ workspace: ws.api, daemon: stubDaemon(new Set()) });
    await selection.load();
    expect(selection.count()).toBe(2);
    await selection.clear();
    expect(selection.values()).toEqual([]);
    expect(ws.writesOf('selected_metarecords')).toEqual([[]]);
    await selection.clear(); // already empty: no second write
    expect(ws.writesOf('selected_metarecords')).toHaveLength(1);
  });

  test('adopting a repository at startup keeps the selection', async () => {
    const ws = stubWorkspace({ selected_metarecords: ['x'] });
    const selection = createMultiSelect({ workspace: ws.api, daemon: stubDaemon(new Set(['x'])) });
    await selection.load(); // active_repo is unset (a workspace before its repo)
    ws.externalSet('active_repo', 'r1');
    expect(selection.values()).toEqual(['x']);
    expect(ws.writesOf('selected_metarecords')).toEqual([]);
  });

  test('switching to another repository clears the selection', async () => {
    const ws = stubWorkspace({ active_repo: 'r1', selected_metarecords: ['x'] });
    const selection = createMultiSelect({ workspace: ws.api, daemon: stubDaemon(new Set(['x'])) });
    await selection.load();
    ws.externalSet('active_repo', 'r2');
    expect(selection.values()).toEqual([]);
    expect(ws.writesOf('selected_metarecords')).toEqual([[]]);
  });

  test('pruneVanished drops the metarecords that no longer exist', async () => {
    const ws = stubWorkspace({ active_repo: 'r1' });
    const existing = new Set(['a', 'b']);
    const daemon = stubDaemon(existing);
    const selection = createMultiSelect({ workspace: ws.api, daemon });
    await selection.load();
    await selection.add(['a', 'b']);
    existing.delete('b'); // deleted elsewhere: `query/delete`, a trashing, the CLI
    await selection.pruneVanished();
    expect(selection.values()).toEqual(['a']);
    expect(ws.writesOf('selected_metarecords')).toEqual([
      ['a', 'b'],
      ['a'],
    ]);
  });

  test('pruneVanished keeps the selection when the daemon cannot answer', async () => {
    const ws = stubWorkspace({ active_repo: 'r1' });
    const daemon = {
      call: vi.fn(async () => {
        throw new Error('daemon down');
      }),
    };
    const selection = createMultiSelect({ workspace: ws.api, daemon });
    await selection.load();
    await selection.add(['a']);
    await selection.pruneVanished();
    expect(selection.values()).toEqual(['a']);
    expect(ws.writesOf('selected_metarecords')).toEqual([['a']]);
  });

  test('pruneVanished asks nothing while nothing is checked', async () => {
    const ws = stubWorkspace({ active_repo: 'r1' });
    const daemon = stubDaemon(new Set());
    const selection = createMultiSelect({ workspace: ws.api, daemon });
    await selection.load();
    await selection.pruneVanished();
    expect(daemon.call).not.toHaveBeenCalled();
  });

  test('pruneVanished skips without a repository (untracked browsing)', async () => {
    const ws = stubWorkspace({ selected_metarecords: ['x'] });
    const daemon = stubDaemon(new Set());
    const selection = createMultiSelect({ workspace: ws.api, daemon });
    await selection.load();
    await selection.pruneVanished();
    expect(daemon.call).not.toHaveBeenCalled();
    expect(selection.values()).toEqual(['x']);
  });
});
