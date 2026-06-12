// Core of the shell⇄panel postMessage protocol (lib/panels/bridge.ts):
// source validation, request routing to Tauri commands, subscription
// forwarding, and command dispatch. DOM/iframe handling lives in
// PanelHost and is exercised in the running app.

import { describe, expect, test, vi } from 'vitest';
import { createBridgeCore } from '../src/lib/panels/bridge';

function setup() {
  const invoke = vi.fn(async (_cmd: string, _args?: unknown) => 'ok' as unknown);
  const dispatch = vi.fn(async (_invocation: string) => {});
  const onCommandsChanged = vi.fn();
  const onPendingKeys = vi.fn();
  const bridge = createBridgeCore({ invoke, dispatch, onCommandsChanged, onPendingKeys });

  const source = 'ws-1|hello'; // string instance id
  const post = vi.fn();
  bridge.register(source, { wsId: 'ws-1', panelType: 'hello' }, post);
  return { bridge, invoke, dispatch, onCommandsChanged, onPendingKeys, source, post };
}

const request = (id: number, method: string, params: unknown) => ({
  mf: true,
  type: 'request',
  id,
  method,
  params,
});

describe('bridge core', () => {
  test('messages from unregistered sources are ignored', async () => {
    const { bridge, invoke } = setup();
    const stranger = 'ws-9|nope';
    await bridge.onMessage(stranger, request(1, 'workspace.get', { key: 'k' }));
    expect(invoke).not.toHaveBeenCalled();
  });

  test('workspace.get routes to ws_get_var with the panel workspace', async () => {
    const { bridge, invoke, source, post } = setup();
    invoke.mockResolvedValueOnce(['/tmp/a']);
    await bridge.onMessage(source, request(7, 'workspace.get', { key: 'selected_paths' }));

    expect(invoke).toHaveBeenCalledWith('ws_get_var', { wsId: 'ws-1', key: 'selected_paths' });
    expect(post).toHaveBeenCalledWith({
      mf: true,
      type: 'response',
      id: 7,
      ok: true,
      result: ['/tmp/a'],
    });
  });

  test('workspace.set routes to ws_set_var', async () => {
    const { bridge, invoke, source } = setup();
    await bridge.onMessage(source, request(1, 'workspace.set', { key: 'k', value: 42 }));
    expect(invoke).toHaveBeenCalledWith('ws_set_var', { wsId: 'ws-1', key: 'k', value: 42 });
  });

  test('failures become error responses', async () => {
    const { bridge, invoke, source, post } = setup();
    invoke.mockRejectedValueOnce('unknown workspace: ws-1');
    await bridge.onMessage(source, request(3, 'workspace.get', { key: 'k' }));
    expect(post).toHaveBeenCalledWith({
      mf: true,
      type: 'response',
      id: 3,
      ok: false,
      error: 'unknown workspace: ws-1',
    });
  });

  test('unknown methods are rejected', async () => {
    const { bridge, source, post } = setup();
    await bridge.onMessage(source, request(4, 'nope.nope', {}));
    expect(post).toHaveBeenCalledWith(
      expect.objectContaining({ type: 'response', id: 4, ok: false }),
    );
  });

  test('subscriptions deliver var changes for the right workspace only', async () => {
    const { bridge, source, post } = setup();
    await bridge.onMessage(source, request(1, 'workspace.subscribe', { key: 'selected_metarecord' }));

    bridge.forwardVarChange('ws-1', 'selected_metarecord', { uuid: 'u1' });
    bridge.forwardVarChange('ws-2', 'selected_metarecord', { uuid: 'u2' });
    bridge.forwardVarChange('ws-1', 'other_key', 1);

    const pushes = post.mock.calls
      .map(([msg]) => msg)
      .filter((msg) => msg.type === 'var-changed');
    expect(pushes).toEqual([
      { mf: true, type: 'var-changed', key: 'selected_metarecord', value: { uuid: 'u1' } },
    ]);
  });

  test('a wildcard subscription receives every var of the workspace', async () => {
    const { bridge, source, post } = setup();
    await bridge.onMessage(source, request(1, 'workspace.subscribe', { key: '*' }));
    bridge.forwardVarChange('ws-1', 'anything', 5);
    const pushes = post.mock.calls.map(([m]) => m).filter((m) => m.type === 'var-changed');
    expect(pushes).toEqual([{ mf: true, type: 'var-changed', key: 'anything', value: 5 }]);
  });

  test('workspace.adoptRepo routes to adopt_repo', async () => {
    const { bridge, invoke, source } = setup();
    await bridge.onMessage(source, request(1, 'workspace.adoptRepo', { repo: 'r-9' }));
    expect(invoke).toHaveBeenCalledWith('adopt_repo', { wsId: 'ws-1', repo: 'r-9' });
  });

  test('commands.register forwards to the registry and refreshes', async () => {
    const { bridge, invoke, onCommandsChanged, source } = setup();
    await bridge.onMessage(
      source,
      request(1, 'commands.register', {
        name: 'hello:greet',
        label: 'Greet',
        scope: 'local',
        reveal: true,
      }),
    );
    expect(invoke).toHaveBeenCalledWith('register_command', {
      panelType: 'hello',
      name: 'hello:greet',
      label: 'Greet',
      scope: 'local',
      reveal: true,
    });
    expect(onCommandsChanged).toHaveBeenCalled();
  });

  test('commands.invoke goes through the shell dispatcher', async () => {
    const { bridge, dispatch, source } = setup();
    await bridge.onMessage(source, request(1, 'commands.invoke', { invocation: 'tab:new' }));
    expect(dispatch).toHaveBeenCalledWith('tab:new');
  });

  test('key-pending forwards the panel matcher state to the shell hint', async () => {
    const { bridge, onPendingKeys, source } = setup();
    const pending = { prefix: ['s'], candidates: [{ keys: ['s', 'l'], invocation: 'panel:set-type metarecord-list' }] };
    await bridge.onMessage(source, { mf: true, type: 'key-pending', pending });
    expect(onPendingKeys).toHaveBeenCalledWith(pending);
    await bridge.onMessage(source, { mf: true, type: 'key-pending', pending: null });
    expect(onPendingKeys).toHaveBeenLastCalledWith(null);
  });

  test('key-resolved goes through the shell dispatcher', async () => {
    const { bridge, dispatch, source } = setup();
    await bridge.onMessage(source, { mf: true, type: 'key-resolved', invocation: 'tab:next' });
    expect(dispatch).toHaveBeenCalledWith('tab:next');
  });

  test('addKeybinding defaults the scope to the panel type', async () => {
    const { bridge, invoke, source } = setup();
    await bridge.onMessage(
      source,
      request(1, 'addKeybinding', { invocation: 'hello:greet', combo: 'ctrl+g' }),
    );
    expect(invoke).toHaveBeenCalledWith('suggest_keybinding', {
      combo: 'ctrl+g',
      invocation: 'hello:greet',
      when: 'hello',
      textInput: false,
    });
  });

  test('statusBar.message posts to the panel workspace', async () => {
    const { bridge, invoke, source } = setup();
    await bridge.onMessage(
      source,
      request(1, 'statusBar.message', { text: 'Hi', timeoutMs: 3000 }),
    );
    expect(invoke).toHaveBeenCalledWith('post_status', {
      wsId: 'ws-1',
      text: 'Hi',
      kind: 'info',
      timeoutMs: 3000,
    });
  });

  test('dispatchCommand sends command messages and resolves on result', async () => {
    const { bridge, source, post } = setup();
    const done = bridge.dispatchCommand(source, 'hello:greet', ['now']);

    const sent = post.mock.calls.map(([m]) => m).find((m) => m.type === 'command');
    expect(sent).toMatchObject({ name: 'hello:greet', args: ['now'] });

    await bridge.onMessage(source, {
      mf: true,
      type: 'command-result',
      invocationId: sent.invocationId,
      ok: true,
    });
    await expect(done).resolves.toBeUndefined();
  });

  test('unregister stops delivery', async () => {
    const { bridge, source, post } = setup();
    await bridge.onMessage(source, request(1, 'workspace.subscribe', { key: '*' }));
    bridge.unregister(source);
    bridge.forwardVarChange('ws-1', 'k', 1);
    const pushes = post.mock.calls.map(([m]) => m).filter((m) => m.type === 'var-changed');
    expect(pushes).toEqual([]);
  });
});
