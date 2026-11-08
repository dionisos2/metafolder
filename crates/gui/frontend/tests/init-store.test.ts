// `initStore`: the shell listens before it reads the initial state, and an
// event that arrives while the read is in flight is applied over it — the Rust
// side announces a change once, and what it announced before the shell
// listened is in the initial state instead (doc "GUI").

import { beforeEach, describe, expect, test, vi } from 'vitest';

const handlers = new Map<string, (event: { payload: unknown }) => void>();
const calls: string[] = [];
let initial: Record<string, unknown> = {};
// Run while `get_initial_state` is in flight, as an event racing the read.
let duringRead: () => void = () => {};

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async (command: string) => {
    calls.push(command);
    if (command === 'get_initial_state') {
      duringRead();
      return initial;
    }
    return null;
  }),
  listen: vi.fn(async (name: string, handler: (event: { payload: unknown }) => void) => {
    calls.push(`listen:${name}`);
    handlers.set(name, handler);
    return () => {};
  }),
}));

vi.mock('../src/lib/panels/api', () => ({ startChangePolling: () => {} }));

const { initStore, store } = await import('../src/lib/store.svelte');

const slot = (workspace_id: string | null) => ({ visible: true, workspace_id, panel_type: null });

function initialState(extra: Record<string, unknown> = {}) {
  return {
    workspaces: [{ id: 'ws-1', name: 'Workspace 1', active_repo: null, repo_name: null }],
    layout: { left: slot('ws-1'), right: slot(null), focused: 'left' },
    keybindings: [],
    commands: [],
    panel_types: [],
    style_css: '',
    gui_port: 7524,
    daemon_url: '',
    page_sizes: {},
    panel_settings: {},
    panel_defaults: {},
    session_token: '',
    daemon_health: null,
    questions: { questions: [], script_keys: true },
    script_tasks: { tasks: [] },
    ...extra,
  };
}

describe('initStore', () => {
  beforeEach(() => {
    handlers.clear();
    calls.length = 0;
    duringRead = () => {};
    initial = initialState();
    store.daemonConnected = true;
    store.ui.questions = [];
    store.ui.scriptTasks = [];
  });

  test('it listens before it reads the initial state', async () => {
    await initStore();
    const read = calls.indexOf('get_initial_state');
    for (const name of ['layout-changed', 'workspaces-changed', 'daemon-health-changed', 'input-wait-changed', 'script-task-changed']) {
      expect(calls.indexOf(`listen:${name}`)).toBeGreaterThanOrEqual(0);
      expect(calls.indexOf(`listen:${name}`)).toBeLessThan(read);
    }
  });

  test('the daemon found unreachable before the shell listened shows', async () => {
    initial = initialState({
      daemon_health: { connected: false, compatible: false, daemon_api_version: null, gui_api_version: 25 },
    });
    await initStore();
    expect(store.daemonConnected).toBe(false);
  });

  test('the questions and scripts already up show', async () => {
    initial = initialState({
      questions: { questions: [{ id: 3, prompt: 'Go?', keys: ['y'], workspace: 'ws-1', task: 't' }], script_keys: false },
      script_tasks: { tasks: [{ task: 't', workspace_id: 'ws-1', label: 'a.sh' }] },
    });
    await initStore();
    expect(store.ui.questions.map((q) => q.id)).toEqual([3]);
    expect(store.ui.scriptKeys).toBe(false);
    expect(store.ui.scriptTasks.map((t) => t.task)).toEqual(['t']);
  });

  test('an event arriving during the read is applied over it', async () => {
    duringRead = () => {
      handlers.get('layout-changed')!({
        payload: { left: slot('ws-2'), right: slot(null), focused: 'left' },
      });
      handlers.get('daemon-health-changed')!({ payload: { connected: false } });
    };
    await initStore();
    expect(store.layout.left.workspace_id).toBe('ws-2');
    expect(store.daemonConnected).toBe(false);
  });
});
