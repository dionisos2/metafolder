// The per-panel metafolder API factory (lib/panels/api.ts): asserts each
// method maps to the right Tauri command (direct invoke, no postMessage) and
// that the shell-pushed changes reach the panel's registered listeners.

import { describe, expect, test, vi } from 'vitest';
import { createPanelApi } from '../src/lib/panels/api';

function setup() {
  const invoke = vi.fn(async (_cmd: string, _args?: unknown) => ({ status: 200, body: null }) as unknown);
  const dispatch = vi.fn(async (_invocation: string) => {});
  const registerHandler = vi.fn();
  const onCommandsChanged = vi.fn();
  const addDefaultMenuItems = vi.fn();

  let visible = false;
  const visibilityGate = {
    get visible() {
      return visible;
    },
    set(next: boolean) {
      visible = next;
    },
    whenVisible: vi.fn(),
  };

  const instance = createPanelApi(
    { invoke, dispatch, registerHandler, onCommandsChanged, addDefaultMenuItems },
    {
      wsId: 'ws-1',
      panelType: 'metarecord-list',
      guiServer: 'http://127.0.0.1:7524',
      root: {} as ShadowRoot,
      visibilityGate,
    },
  );
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const api = instance.api as any;
  return { instance, api, invoke, dispatch, registerHandler, onCommandsChanged, addDefaultMenuItems, visibilityGate };
}

describe('panel api — identity', () => {
  test('exposes the panel context via getters', () => {
    const { api } = setup();
    expect(api.workspaceId).toBe('ws-1');
    expect(api.panelType).toBe('metarecord-list');
    expect(api.guiServer).toBe('http://127.0.0.1:7524');
  });

  test('ready resolves immediately (mount runs post-init)', async () => {
    const { api } = setup();
    await expect(api.ready).resolves.toBeUndefined();
  });
});

describe('panel api — daemon', () => {
  test('request routes to daemon_request and returns the response', async () => {
    const { api, invoke } = setup();
    invoke.mockResolvedValueOnce({ status: 200, body: { ok: 1 } });
    const res = await api.daemon.request('GET', '/repos');
    expect(invoke).toHaveBeenCalledWith('daemon_request', { method: 'GET', path: '/repos', body: null });
    expect(res).toEqual({ status: 200, body: { ok: 1 } });
  });

  test('call returns the body on success', async () => {
    const { api, invoke } = setup();
    invoke.mockResolvedValueOnce({ status: 200, body: { uuid: 'x' } });
    await expect(api.daemon.call('GET', '/repos/r/metarecords/x')).resolves.toEqual({ uuid: 'x' });
  });

  test('call throws on status >= 400 with the daemon error', async () => {
    const { api, invoke } = setup();
    invoke.mockResolvedValueOnce({ status: 404, body: { error: 'not found' } });
    await expect(api.daemon.call('GET', '/repos/r/metarecords/x')).rejects.toThrow('not found');
  });

  test('repoRoot caches GET /repos across calls', async () => {
    const { api, invoke } = setup();
    invoke.mockResolvedValue({ status: 200, body: [{ repo_uuid: 'r', root: '/tmp/r' }] });
    expect(await api.daemon.repoRoot('r')).toBe('/tmp/r');
    expect(await api.daemon.repoRoot('r')).toBe('/tmp/r');
    const repoCalls = invoke.mock.calls.filter((c) => c[0] === 'daemon_request' && (c[1] as { path: string }).path === '/repos');
    expect(repoCalls).toHaveLength(1);
  });
});

describe('panel api — workspace', () => {
  test('get/set route to ws_get_var/ws_set_var with the panel workspace', async () => {
    const { api, invoke } = setup();
    await api.workspace.get('selected_metarecord');
    expect(invoke).toHaveBeenCalledWith('ws_get_var', { wsId: 'ws-1', key: 'selected_metarecord' });
    await api.workspace.set('k', 42);
    expect(invoke).toHaveBeenCalledWith('ws_set_var', { wsId: 'ws-1', key: 'k', value: 42 });
  });

  test('onChange listeners fire on pushVarChanged (and * receives the key)', () => {
    const { api, instance } = setup();
    const direct = vi.fn();
    const wildcard = vi.fn();
    api.workspace.onChange('selected_metarecord', direct);
    api.workspace.onChange('*', wildcard);
    instance.pushVarChanged('selected_metarecord', { uuid: 'x' });
    expect(direct).toHaveBeenCalledWith({ uuid: 'x' });
    expect(wildcard).toHaveBeenCalledWith({ uuid: 'x' }, 'selected_metarecord');
  });
});

describe('panel api — commands & keybindings', () => {
  test('register stores the handler and registers metadata', () => {
    const { api, invoke, registerHandler, onCommandsChanged } = setup();
    const handler = vi.fn();
    api.commands.register('metarecord-list:next', { label: 'Next', handler });
    expect(registerHandler).toHaveBeenCalledWith('metarecord-list:next', handler);
    expect(invoke).toHaveBeenCalledWith('register_command', {
      panelType: 'metarecord-list',
      name: 'metarecord-list:next',
      label: 'Next',
      reveal: false,
      log: true,
    });
    expect(onCommandsChanged).toHaveBeenCalled();
  });

  test('invoke routes to dispatch', async () => {
    const { api, dispatch } = setup();
    await api.commands.invoke('panel:split');
    expect(dispatch).toHaveBeenCalledWith('panel:split');
  });

  test('addKeybinding defaults when to the panel type', async () => {
    const { api, invoke } = setup();
    await api.addKeybinding('metarecord-list:next', 'j');
    expect(invoke).toHaveBeenCalledWith('suggest_keybinding', {
      combo: 'j',
      invocation: 'metarecord-list:next',
      when: 'metarecord-list',
      textInput: false,
    });
  });
});

describe('panel api — misc surface', () => {
  test('query parse/expand route locally', async () => {
    const { api, invoke } = setup();
    await api.query.parse('a = 1');
    expect(invoke).toHaveBeenCalledWith('parse_query', { dsl: 'a = 1' });
    await api.query.expand('jazz');
    expect(invoke).toHaveBeenCalledWith('expand_query', { text: 'jazz' });
  });

  test('fs and statusBar route to their commands', async () => {
    const { api, invoke } = setup();
    await api.fs.readDir('/tmp');
    expect(invoke).toHaveBeenCalledWith('fs_read_dir', { path: '/tmp' });
    await api.fs.homeDir();
    expect(invoke).toHaveBeenCalledWith('fs_home_dir');
    await api.statusBar.message('hi', 3000);
    expect(invoke).toHaveBeenCalledWith('post_status', { wsId: 'ws-1', text: 'hi', kind: 'info', timeoutMs: 3000 });
  });

  test('messages.onAppend fires on pushMessageAppended', () => {
    const { api, instance } = setup();
    const listener = vi.fn();
    api.messages.onAppend(listener);
    instance.pushMessageAppended({ text: 'x' });
    expect(listener).toHaveBeenCalledWith({ text: 'x' });
  });

  test('bench.record forwards to bench_record', () => {
    const { api, invoke } = setup();
    api.bench.record('mf:list:render', 2.5);
    expect(invoke).toHaveBeenCalledWith('bench_record', { name: 'mf:list:render', durationMs: 2.5 });
  });

  test('pushVisibility updates the gate and notifies onVisibility listeners', () => {
    const { api, instance, visibilityGate } = setup();
    const listener = vi.fn();
    api.onVisibility(listener);
    instance.pushVisibility(true, 'left');
    expect(visibilityGate.visible).toBe(true);
    expect(listener).toHaveBeenCalledWith(true, 'left');
    expect(api.visible).toBe(true);
  });
});
