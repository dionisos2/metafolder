// `panel:open <where> file|metarecord-detail` opens a viewer of the selection
// (doc "Opening a row"). A viewer with nothing of the selection to show is not
// opened: a metarecord without a file has nothing for the viewer panel, a path
// without a metarecord (an untracked file) nothing for the detail panel. With
// no selection at all, a viewer opens as any panel type does (a layout being
// set up, `b t f` on an empty workspace).

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { dispatch } from '../src/lib/commands';
import { setFunctionProvider, type PanelFunctions } from '../src/lib/panelFunctions';
import { store } from '../src/lib/store.svelte';
import type { ExecContext } from '../src/lib/types';

const { tauri, vars } = vi.hoisted(() => ({
  tauri: [] as { cmd: string; args: Record<string, unknown> }[],
  vars: new Map<string, unknown>(),
}));

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async (cmd: string, args: Record<string, unknown>) => {
    tauri.push({ cmd, args });
    if (cmd === 'ws_get_var') return vars.get(`${String(args.wsId)}/${String(args.key)}`) ?? null;
    if (cmd === 'workspace_fork') return 'ws-fork';
    return null;
  }),
  listen: vi.fn(async () => () => {}),
}));

const CONTEXT: ExecContext = { ws: 'ws-1', slot: 'left' };

function select(metarecord: unknown, paths: string[]) {
  vars.set('ws-1/selected_metarecord', metarecord);
  vars.set('ws-1/selected_paths', paths);
}

const opened = () =>
  tauri.filter((c) => c.cmd === 'panel_set_type').map((c) => c.args.panelType);
const statuses = () =>
  tauri.filter((c) => c.cmd === 'post_status').map((c) => String(c.args.text));

describe('panel:open on a viewer of the selection', () => {
  beforeEach(() => {
    tauri.length = 0;
    vars.clear();
  });

  test('a metarecord with a file opens in either viewer', async () => {
    select({ uuid: 'aa', repo: 'r' }, ['/repo/a.txt']);
    await dispatch('panel:open other viewer', CONTEXT);
    await dispatch('panel:open other metarecord-detail', CONTEXT);
    expect(opened()).toEqual(['viewer', 'metarecord-detail']);
  });

  test('a metarecord without a file is not opened in the viewer panel', async () => {
    select({ uuid: 'aa', repo: 'r' }, []);
    await dispatch('panel:open other viewer', CONTEXT);
    expect(opened()).toEqual([]);
    expect(statuses()).toEqual([expect.stringContaining('no file')]);
    // Nor moved anywhere first: no fork, no slot switch.
    expect(tauri.map((c) => c.cmd)).not.toContain('workspace_fork');
    expect(tauri.map((c) => c.cmd)).not.toContain('tab_assign');
  });

  test('a path without a metarecord is not opened in the detail panel', async () => {
    select(null, ['/repo/untracked.txt']);
    await dispatch('panel:open new metarecord-detail', CONTEXT);
    expect(opened()).toEqual([]);
    expect(statuses()).toEqual([expect.stringContaining('no metarecord')]);
    expect(tauri.map((c) => c.cmd)).not.toContain('workspace_fork');
  });

  test('an untracked path still opens in the viewer panel', async () => {
    select(null, ['/repo/untracked.txt']);
    await dispatch('panel:open other viewer', CONTEXT);
    expect(opened()).toEqual(['viewer']);
  });

  test('with nothing selected, a viewer opens as any panel type does', async () => {
    await dispatch('panel:open here viewer', CONTEXT);
    await dispatch('panel:open here metarecord-detail', CONTEXT);
    expect(opened()).toEqual(['viewer', 'metarecord-detail']);
  });

  test('other panel types are never checked', async () => {
    select({ uuid: 'aa', repo: 'r' }, []);
    await dispatch('panel:open other metarecord-list', CONTEXT);
    expect(opened()).toEqual(['metarecord-list']);
  });
});

// The row under the cursor of the focused panel is what a viewer opens (doc
// "Opening a row"). A list keeps its cursor when another panel selects what it
// has not loaded, so the two can differ: the key selects the row again first,
// as the row's open buttons do.
describe('panel:open from a panel with a cursor', () => {
  const calls: string[] = [];

  beforeEach(() => {
    tauri.length = 0;
    vars.clear();
    calls.length = 0;
    store.layout.left = { ...store.layout.left, workspace_id: 'ws-1', panel_type: 'metarecord-list' };
    store.layout.focused = 'left';
  });

  afterEach(() => setFunctionProvider(null));

  function provide(functions: PanelFunctions | null) {
    setFunctionProvider({
      get: async (_ws, type) => (type === 'metarecord-list' ? functions : null),
      provided: () => undefined,
      unsaved: () => [],
      root: () => null,
    });
  }

  test('selects the row under the cursor before opening the viewer', async () => {
    // Selected elsewhere: a metarecord without a file, which the list has not loaded.
    select({ uuid: 'elsewhere', repo: 'r' }, []);
    provide({
      index: () => 2,
      count: () => 5,
      select: (index) => {
        calls.push(`select ${index}`);
        select({ uuid: 'row-2', repo: 'r' }, ['/repo/row-2.txt']);
      },
    });

    await dispatch('panel:open other viewer', CONTEXT);

    expect(calls).toEqual(['select 2']);
    expect(opened()).toEqual(['viewer']);
  });

  test('and before the detail panel', async () => {
    select(null, ['/repo/untracked.txt']);
    provide({
      index: () => 0,
      count: () => 1,
      select: () => select({ uuid: 'row-0', repo: 'r' }, ['/repo/row-0.txt']),
    });

    await dispatch('panel:open other metarecord-detail', CONTEXT);

    expect(opened()).toEqual(['metarecord-detail']);
  });

  test('no row under the cursor leaves the selection as it is', async () => {
    select({ uuid: 'aa', repo: 'r' }, ['/repo/a.txt']);
    provide({ index: () => -1, count: () => 0, select: () => calls.push('select') });

    await dispatch('panel:open other viewer', CONTEXT);

    expect(calls).toEqual([]);
    expect(opened()).toEqual(['viewer']);
  });

  test('a panel type that is no viewer of the selection moves nothing', async () => {
    provide({ index: () => 1, count: () => 2, select: () => calls.push('select') });

    await dispatch('panel:open other log', CONTEXT);

    expect(calls).toEqual([]);
  });
});
