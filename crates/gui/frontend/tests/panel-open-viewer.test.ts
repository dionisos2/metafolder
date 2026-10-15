// `panel:open <where> file|metarecord-detail` opens a viewer of the selection
// (doc "Opening a row"). A viewer with nothing of the selection to show is not
// opened: a metarecord without a file has nothing for the file panel, a path
// without a metarecord (an untracked file) nothing for the detail panel. With
// no selection at all, a viewer opens as any panel type does (a layout being
// set up, `b t f` on an empty workspace).

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { dispatch } from '../src/lib/commands';
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
    await dispatch('panel:open other file', CONTEXT);
    await dispatch('panel:open other metarecord-detail', CONTEXT);
    expect(opened()).toEqual(['file', 'metarecord-detail']);
  });

  test('a metarecord without a file is not opened in the file panel', async () => {
    select({ uuid: 'aa', repo: 'r' }, []);
    await dispatch('panel:open other file', CONTEXT);
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

  test('an untracked path still opens in the file panel', async () => {
    select(null, ['/repo/untracked.txt']);
    await dispatch('panel:open other file', CONTEXT);
    expect(opened()).toEqual(['file']);
  });

  test('with nothing selected, a viewer opens as any panel type does', async () => {
    await dispatch('panel:open here file', CONTEXT);
    await dispatch('panel:open here metarecord-detail', CONTEXT);
    expect(opened()).toEqual(['file', 'metarecord-detail']);
  });

  test('other panel types are never checked', async () => {
    select({ uuid: 'aa', repo: 'r' }, []);
    await dispatch('panel:open other metarecord-list', CONTEXT);
    expect(opened()).toEqual(['metarecord-list']);
  });
});
