// The per-panel metafolder API factory (lib/panels/api.ts): asserts each
// method maps to the right Tauri command (direct invoke, no postMessage) and
// that the shell-pushed changes reach the panel's registered listeners.

import { describe, expect, test, vi } from 'vitest';
import { argSpecFor, clearArgSpecs } from '../src/lib/commands';
import { createPanelApi } from '../src/lib/panels/api';

function setup() {
  const invoke = vi.fn(async (_cmd: string, _args?: unknown) => ({ status: 200, body: null }) as unknown);
  const dispatch = vi.fn(async (_invocation: string) => {});
  const registerHandler = vi.fn();
  const registerArgs = vi.fn();
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
    { invoke, dispatch, registerHandler, registerArgs, onCommandsChanged, addDefaultMenuItems },
    {
      wsId: 'ws-1',
      panelType: 'metarecord-list',
      guiServer: 'http://127.0.0.1:7524',
      sessionToken: 'test-token',
      root: {} as ShadowRoot,
      visibilityGate,
    },
  );
  const api = instance.api as any;
  return { instance, api, invoke, dispatch, registerHandler, registerArgs, onCommandsChanged, addDefaultMenuItems, visibilityGate };
}

describe('panel api — sync', () => {
  test('each sync method maps to its Tauri command with the right args', async () => {
    const { api, invoke } = setup();

    await api.sync.status('a', 'b');
    expect(invoke).toHaveBeenCalledWith('sync_status', { repoA: 'a', repoB: 'b' });

    await api.sync.link('a', 'b', 'ua', 'ub', 'a');
    expect(invoke).toHaveBeenCalledWith('sync_link', {
      repoA: 'a',
      repoB: 'b',
      uuidA: 'ua',
      uuidB: 'ub',
      host: 'a',
    });

    await api.sync.unlink('a', 'b', 'lnk');
    expect(invoke).toHaveBeenCalledWith('sync_unlink', {
      repoA: 'a',
      repoB: 'b',
      link: 'lnk',
      withEndpoint: undefined,
    });

    await api.sync.plan('a', 'b', '/tmp/intents.toml', undefined, 'skip');
    expect(invoke).toHaveBeenCalledWith('sync_plan', {
      repoA: 'a',
      repoB: 'b',
      intentsPath: '/tmp/intents.toml',
      host: undefined,
      onConflict: 'skip',
    });

    await api.sync.run('a', 'b');
    expect(invoke).toHaveBeenCalledWith('sync_run', { repoA: 'a', repoB: 'b' });

    await api.sync.show('a', 'b', true, false);
    expect(invoke).toHaveBeenCalledWith('sync_show', {
      repoA: 'a',
      repoB: 'b',
      conflicts: true,
      files: false,
    });
  });
});

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

  test('settings maps kebab config keys to a frozen camelCase object', () => {
    const noop = vi.fn();
    const gate = { get visible() { return false; }, set() {}, whenVisible: vi.fn() };
    const instance = createPanelApi(
      { invoke: vi.fn(), dispatch: vi.fn(), registerHandler: noop, registerArgs: noop, onCommandsChanged: noop, addDefaultMenuItems: noop },
      {
        wsId: 'ws-1',
        panelType: 'metarecord-list',
        guiServer: 'http://127.0.0.1:7524',
        sessionToken: 't',
        panelSettings: { 'finder-debounce-ms': 900, 'status-error-ms': 4000 },
        root: {} as ShadowRoot,
        visibilityGate: gate,
      },
    );
    const settings = (instance.api as any).settings;
    expect(settings.finderDebounceMs).toBe(900);
    expect(settings.statusErrorMs).toBe(4000);
    // Unspecified keys are undefined (panels fall back to their own default).
    expect(settings.taskPollMs).toBeUndefined();
    expect(Object.isFrozen(settings)).toBe(true);
  });

  test('settings is defined even without a panelSettings context', () => {
    const { api } = setup();
    expect(api.settings.finderDebounceMs).toBeUndefined();
  });

  test('defaults exposes this panel type\'s config table, camelCased and frozen', () => {
    const noop = vi.fn();
    const gate = { get visible() { return false; }, set() {}, whenVisible: vi.fn() };
    const instance = createPanelApi(
      { invoke: vi.fn(), dispatch: vi.fn(), registerHandler: noop, registerArgs: noop, onCommandsChanged: noop, addDefaultMenuItems: noop },
      {
        wsId: 'ws-1',
        panelType: 'metarecord-list',
        guiServer: 'http://127.0.0.1:7524',
        sessionToken: 't',
        panelDefaults: {
          columns: 'name mfr_type',
          'finder-fields': ['label:direct'],
          'text-preview-limit': 42,
        },
        root: {} as ShadowRoot,
        visibilityGate: gate,
      },
    );
    const defaults = (instance.api as any).defaults;
    // Kebab keys become camelCase; values (strings, arrays, numbers) pass through.
    expect(defaults.columns).toBe('name mfr_type');
    expect(defaults.finderFields).toEqual(['label:direct']);
    expect(defaults.textPreviewLimit).toBe(42);
    // Unconfigured keys are undefined (the panel falls back to its own constant).
    expect(defaults.gridNameColumn).toBeUndefined();
    expect(Object.isFrozen(defaults)).toBe(true);
  });

  test('defaults is an empty object without a panelDefaults context', () => {
    const { api } = setup();
    expect(api.defaults).toEqual({});
    expect(api.defaults.columns).toBeUndefined();
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

  /** Answers the change feed's baseline poll, and `res` to everything else. */
  function answering(invoke: ReturnType<typeof setup>['invoke'], res: unknown) {
    invoke.mockImplementation(async (_cmd: string, args?: unknown) =>
      (args as { path?: string })?.path?.includes('/log/since')
        ? { status: 200, body: { head: 1, operations: [] } }
        : res,
    );
  }

  /** A daemon whose query calls hang until `daemon_abort` names them, and then
   *  fail as the proxy fails them ("aborted"). */
  function hangingUntilAborted(invoke: ReturnType<typeof setup>['invoke']) {
    const pending = new Map<string, (e: unknown) => void>();
    invoke.mockImplementation(async (cmd: string, args?: unknown) => {
      const a = args as { path?: string; abortId?: string; id?: string };
      if (cmd === 'daemon_abort') {
        pending.get(a.id!)?.('aborted');
        return pending.delete(a.id!);
      }
      if (a.path?.includes('/log/since')) return { status: 200, body: { head: 1, operations: [] } };
      if (a.abortId === undefined) return { status: 200, body: { results: [] } };
      return new Promise((_resolve, reject) => pending.set(a.abortId!, reject));
    });
  }

  test('an aborted query is dropped by the daemon proxy and rejects as an AbortError', async () => {
    const { api, invoke } = setup();
    hangingUntilAborted(invoke);
    const controller = new AbortController();
    const reading = api.daemon.query('r', { query: null }, { signal: controller.signal });
    await vi.waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('daemon_request', expect.objectContaining({ path: '/repos/r/query' })),
    );
    const sent = invoke.mock.calls.find(([, a]) => (a as { path?: string })?.path === '/repos/r/query');
    const abortId = (sent![1] as { abortId?: string }).abortId;
    expect(abortId).toBeTypeOf('string');

    controller.abort();
    await expect(reading).rejects.toMatchObject({ name: 'AbortError' });
    expect(invoke).toHaveBeenCalledWith('daemon_abort', { id: abortId });
  });

  test('a signal already aborted sends nothing', async () => {
    const { api, invoke } = setup();
    hangingUntilAborted(invoke);
    const controller = new AbortController();
    controller.abort();
    await expect(
      api.daemon.call('POST', '/repos/r/query', { query: null }, { signal: controller.signal }),
    ).rejects.toMatchObject({ name: 'AbortError' });
    expect(invoke).not.toHaveBeenCalledWith('daemon_request', expect.objectContaining({ path: '/repos/r/query' }));
  });

  test('two abortable calls get two ids', async () => {
    const { api, invoke } = setup();
    answering(invoke, { status: 200, body: { results: [] } });
    const signal = new AbortController().signal;
    await api.daemon.query('r', { query: null }, { signal });
    await api.daemon.query('r', { query: null }, { signal });
    const ids = invoke.mock.calls
      .filter(([, a]) => (a as { path?: string })?.path === '/repos/r/query')
      .map(([, a]) => (a as { abortId?: string }).abortId);
    expect(ids).toHaveLength(2);
    expect(ids[0]).not.toEqual(ids[1]);
  });

  test('call returns the body on success', async () => {
    const { api, invoke } = setup();
    answering(invoke, { status: 200, body: { uuid: 'x' } });
    await expect(api.daemon.call('GET', '/repos/r/metarecords/x')).resolves.toEqual({ uuid: 'x' });
  });

  test('call throws on status >= 400 with the daemon error', async () => {
    const { api, invoke } = setup();
    answering(invoke, { status: 404, body: { error: 'not found' } });
    await expect(api.daemon.call('GET', '/repos/r/metarecords/x')).rejects.toThrow('not found');
  });

  test("a panel's first read of a repository takes the change feed's baseline first", async () => {
    const { api, invoke } = setup();
    answering(invoke, { status: 200, body: { results: [] } });
    await api.daemon.query('fresh', { query: null });
    await api.daemon.query('fresh', { query: null });
    const paths = invoke.mock.calls
      .filter((c) => c[0] === 'daemon_request')
      .map((c) => (c[1] as { path: string }).path);
    expect(paths).toEqual(['/repos/fresh/log/since', '/repos/fresh/query', '/repos/fresh/query']);
  });

  test('a read is never served from memory: asking twice asks the daemon twice', async () => {
    const { api, invoke } = setup();
    answering(invoke, { status: 200, body: { u1: ['/a'] } });
    await api.daemon.treePaths('r2', 'mfr_path', ['u1']);
    await api.daemon.call('POST', '/repos/r2/tree/resolve', { uuids: ['u1'] });
    const resolves = invoke.mock.calls.filter(
      (c) => (c[1] as { path?: string })?.path === '/repos/r2/query/fields/resolve-tree',
    );
    expect(resolves).toHaveLength(2);
  });

  test("resolveTreeRef resolves the parent in the value's own field", async () => {
    const { api, invoke } = setup();
    answering(invoke, { status: 200, body: { results: [{ uuid: 'p', paths: ['genre'] }] } });
    await api.daemon.resolveTreeRef('r2', { parent: 'p', name: 'jazz' }, 'tag').catch(() => {});
    const bodies = invoke.mock.calls
      .filter((c) => (c[1] as { path?: string })?.path === '/repos/r2/query/fields/resolve-tree')
      .map((c) => (c[1] as { body: { field: string } }).body.field);
    expect(bodies).toEqual(['tag']);
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
  test('registering a command claims its name first', () => {
    // The shell drops a commands.js entry of that name (doc "User commands")
    // before the panel's handler and arguments are recorded.
    const order: string[] = [];
    const noop = () => {};
    const instance = createPanelApi(
      {
        invoke: vi.fn(async () => null),
        dispatch: vi.fn(),
        claimCommand: (name: string) => void order.push(`claim ${name}`),
        registerHandler: (name: string) => void order.push(`handler ${name}`),
        registerArgs: noop,
        onCommandsChanged: noop,
        addDefaultMenuItems: noop,
      },
      {
        wsId: 'ws-1',
        panelType: 'log',
        guiServer: 'http://127.0.0.1:7524',
        sessionToken: 'test-token',
        root: {} as ShadowRoot,
        visibilityGate: { visible: false, set: noop, whenVisible: noop } as never,
      },
    );
    (instance.api as any).commands.register('log:find', { handler: () => {} });
    expect(order).toEqual(['claim log:find', 'handler log:find']);
  });

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

  test('register forwards declared args to this instance, not a global registry', () => {
    const { api, registerArgs } = setup();
    const args = [{ name: 'field', prompt: () => 'Field?' }];
    api.commands.register('metarecord:set-field', { label: 'Set', args });
    // Per instance, like the handler: the spec closes over *this* panel's
    // state, so another workspace's instance must not overwrite it.
    expect(registerArgs).toHaveBeenCalledWith('metarecord:set-field', args);
    expect(argSpecFor('metarecord:set-field')).toBeUndefined();
    clearArgSpecs();
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
      focus: null,
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

  test('config seeds route to their commands', async () => {
    const { api, invoke } = setup();
    await api.config.pickerSeed('tag');
    expect(invoke).toHaveBeenCalledWith('picker_seed', { field: 'tag' });
    await api.config.refCompletionSeed('tag');
    expect(invoke).toHaveBeenCalledWith('ref_completion_seed', { field: 'tag' });
    await api.config.refSeed('tag');
    expect(invoke).toHaveBeenCalledWith('ref_seed', { field: 'tag' });
    await api.config.labelSeparator();
    expect(invoke).toHaveBeenCalledWith('label_separator');
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

  test('statusBar.error posts kind "error" (so the status bar styles it)', async () => {
    const { api, invoke } = setup();
    await api.statusBar.error(new Error('boom'), 8000);
    expect(invoke).toHaveBeenCalledWith('post_status', {
      wsId: 'ws-1',
      text: 'boom',
      kind: 'error',
      timeoutMs: 8000,
    });
  });

  test('fs write operations route to their commands', async () => {
    const { api, invoke } = setup();
    await api.fs.mkdir('/tmp/d');
    expect(invoke).toHaveBeenCalledWith('fs_mkdir', { path: '/tmp/d' });
    await api.fs.createFile('/tmp/f');
    expect(invoke).toHaveBeenCalledWith('fs_create_file', { path: '/tmp/f' });
    await api.fs.move('/tmp/a', '/tmp/b');
    expect(invoke).toHaveBeenCalledWith('fs_move', { from: '/tmp/a', to: '/tmp/b' });
    await api.fs.copy('/tmp/a', '/tmp/c');
    expect(invoke).toHaveBeenCalledWith('fs_copy', { from: '/tmp/a', to: '/tmp/c' });
    await api.fs.remove('/tmp/a');
    expect(invoke).toHaveBeenCalledWith('fs_delete', { path: '/tmp/a' });
  });

  test('trash routes to the trash commands', async () => {
    const { api, invoke } = setup();
    await api.trash.list('r1');
    expect(invoke).toHaveBeenCalledWith('trash_list', { repo: 'r1' });
    await api.trash.restore('r1', 'id1');
    expect(invoke).toHaveBeenCalledWith('trash_restore', { repo: 'r1', id: 'id1' });
    await api.trash.remove('r1', 'id1');
    expect(invoke).toHaveBeenCalledWith('trash_remove', { repo: 'r1', id: 'id1' });
    await api.trash.empty('r1');
    expect(invoke).toHaveBeenCalledWith('trash_empty', { repo: 'r1' });
    await api.trash.trashPath('r1', '/tmp/r1/song.mp3');
    expect(invoke).toHaveBeenCalledWith('trash_path', { repo: 'r1', path: '/tmp/r1/song.mp3' });
  });

  test('log navigation routes to the coordinated navigation commands', async () => {
    const { api, invoke } = setup();
    await api.log.rollback('r1', { id: 4 });
    expect(invoke).toHaveBeenCalledWith('log_rollback', { repo: 'r1', target: { id: 4 } });
    await api.log.revert('r1', { rev_id: 3 }, true);
    expect(invoke).toHaveBeenCalledWith('log_revert', {
      repo: 'r1',
      target: { rev_id: 3 },
      withDependents: true,
    });
  });

  test('recent routes to the recent commands', async () => {
    const { api, invoke } = setup();
    invoke.mockResolvedValueOnce([{ uuid: 'u1', viewed_at: '2026-08-15T10:00:00Z' }]);
    const entries = await api.recent.list('r1', 20);
    expect(invoke).toHaveBeenCalledWith('recent_read', { repo: 'r1', limit: 20 });
    expect(entries).toEqual([{ uuid: 'u1', viewed_at: '2026-08-15T10:00:00Z' }]);
    await api.recent.touch('r1', 'u1');
    expect(invoke).toHaveBeenCalledWith('recent_touch', { repo: 'r1', uuid: 'u1' });
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
