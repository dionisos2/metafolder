// Only a line the user types — after `!`, or in the shell panel, which goes
// through `!` — runs with their shell configuration; a program started by
// `file:open-with` (like a script) runs in a plain `sh` (doc "shell panel").

import { expect, test, vi } from 'vitest';
import { dispatch } from '../src/lib/commands';
import { store } from '../src/lib/store.svelte';

const { invoked } = vi.hoisted(() => ({ invoked: [] as { cmd: string; args: unknown }[] }));

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async (cmd: string, args: { key?: string }) => {
    invoked.push({ cmd, args });
    return cmd === 'ws_get_var' && args.key === 'selected_paths' ? ['/music/a.flac'] : null;
  }),
  listen: vi.fn(async () => () => {}),
}));

function runShellCalls() {
  return invoked.filter((call) => call.cmd === 'run_shell').map((call) => call.args);
}

test('a typed line is run as typed, a program opened on a file is not', async () => {
  store.workspaces = [{ id: 'ws-1', name: 'music', active_repo: null, repo_name: null }];
  store.layout.left = { visible: true, workspace_id: 'ws-1', panel_type: 'shell' };
  store.layout.focused = 'left';

  await dispatch('!echo hi');
  await dispatch('file:open-with vlc');

  expect(runShellCalls()).toEqual([
    { wsId: 'ws-1', commandLine: 'echo hi', label: null, typed: true },
    { wsId: 'ws-1', commandLine: "vlc '/music/a.flac'", label: null, typed: false },
  ]);
});
