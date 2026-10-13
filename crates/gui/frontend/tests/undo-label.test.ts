// `log:undo` / `log:redo` by label (doc "Undo and redo by label"): a label
// named is undone; none named and one candidate, it is undone at once.

import { describe, expect, test, vi } from 'vitest';
import { dispatch } from '../src/lib/commands';

const { tauri, candidates } = vi.hoisted(() => ({
  tauri: [] as { cmd: string; args: unknown }[],
  candidates: [] as { id: number; name: string }[],
}));

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async (cmd: string, args: unknown) => {
    tauri.push({ cmd, args });
    return cmd === 'log_candidates' ? candidates : null;
  }),
  listen: vi.fn(async () => () => {}),
}));

const navigations = () => tauri.filter((c) => c.cmd === 'log_navigate').map((c) => c.args);

describe('log:undo', () => {
  test('a label named is undone, without asking for the candidates', async () => {
    tauri.length = 0;
    await dispatch('log:undo 12', { ws: 'ws-1', slot: 'left' });
    expect(navigations()).toEqual([{ wsId: 'ws-1', redo: false, label: 12 }]);
    expect(tauri.some((c) => c.cmd === 'log_candidates')).toBe(false);
  });

  test('one candidate is undone at once', async () => {
    tauri.length = 0;
    candidates.splice(0, candidates.length, { id: 7, name: 'mf metarecord add' });
    await dispatch('log:redo', { ws: 'ws-1', slot: 'left' });
    expect(navigations()).toEqual([{ wsId: 'ws-1', redo: true, label: null }]);
  });

  test('something that is not a label is said, and nothing is undone', async () => {
    tauri.length = 0;
    await dispatch('log:undo abc', { ws: 'ws-1', slot: 'left' });
    expect(navigations()).toEqual([]);
  });
});
