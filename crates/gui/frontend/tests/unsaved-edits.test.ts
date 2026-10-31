// Edits in progress (doc "Edits in progress"): the selection does not move away
// from one unless the user agrees, and closing asks first.

import { afterEach, describe, expect, test, vi } from 'vitest';
import { setFunctionProvider } from '../src/lib/panelFunctions';
import { confirmClosing, confirmSelectionWrite } from '../src/lib/unsavedEdits';
import { createPanelApi } from '../src/lib/panels/api';

const WS = 'ws-1';

function provider(edits: { describe: string; discard: () => unknown }[]) {
  setFunctionProvider({
    get: async () => null,
    provided: () => undefined,
    unsaved: () => edits,
  });
}

const tick = () => new Promise((r) => setTimeout(r, 0));

afterEach(async () => {
  await tick();
  setFunctionProvider(null);
  vi.unstubAllGlobals();
});

describe('confirmSelectionWrite', () => {
  test('nothing in progress: the write passes, nothing asked or read', async () => {
    provider([]);
    const ask = vi.fn(() => true);
    vi.stubGlobal('confirm', ask);
    const read = vi.fn();
    expect(await confirmSelectionWrite(WS, 'selected_paths', ['/b'], read)).toBe(true);
    expect(ask).not.toHaveBeenCalled();
    expect(read).not.toHaveBeenCalled();
  });

  test('another variable passes', async () => {
    const discard = vi.fn();
    provider([{ describe: 'a.txt', discard }]);
    vi.stubGlobal('confirm', vi.fn(() => false));
    expect(await confirmSelectionWrite(WS, 'file-manager:dir', '/x', vi.fn())).toBe(true);
  });

  test('the same selection written again asks nothing', async () => {
    const discard = vi.fn();
    provider([{ describe: 'a.txt', discard }]);
    const ask = vi.fn(() => false);
    vi.stubGlobal('confirm', ask);
    expect(await confirmSelectionWrite(WS, 'selected_paths', ['/a'], async () => ['/a'])).toBe(true);
    expect(ask).not.toHaveBeenCalled();
    expect(discard).not.toHaveBeenCalled();
  });

  test('refused: the write does not happen, the edit stays', async () => {
    const discard = vi.fn();
    provider([{ describe: 'a.txt', discard }]);
    vi.stubGlobal('confirm', vi.fn(() => false));
    expect(await confirmSelectionWrite(WS, 'selected_paths', ['/b'], async () => ['/a'])).toBe(false);
    expect(discard).not.toHaveBeenCalled();
  });

  test('agreed: the edit is discarded and the write passes', async () => {
    const discard = vi.fn();
    provider([{ describe: 'a.txt', discard }]);
    const ask = vi.fn(() => true);
    vi.stubGlobal('confirm', ask);
    expect(await confirmSelectionWrite(WS, 'selected_paths', ['/b'], async () => ['/a'])).toBe(true);
    expect(discard).toHaveBeenCalledOnce();
    expect(ask.mock.calls[0]).toEqual([expect.stringContaining('a.txt')]);
  });

  test('a writer of the pair asks once: refusing the first refuses the second', async () => {
    provider([{ describe: 'a.txt', discard: vi.fn() }]);
    const ask = vi.fn(() => false);
    vi.stubGlobal('confirm', ask);
    const read = async () => 'old';
    expect(await confirmSelectionWrite(WS, 'selected_metarecord', 'new', read)).toBe(false);
    expect(await confirmSelectionWrite(WS, 'selected_paths', ['/b'], read)).toBe(false);
    expect(ask).toHaveBeenCalledOnce();

    // A later move is a new question.
    await tick();
    await confirmSelectionWrite(WS, 'selected_paths', ['/c'], read);
    expect(ask).toHaveBeenCalledTimes(2);
  });
});

describe('confirmClosing', () => {
  test('nothing in progress: closes without asking', () => {
    provider([]);
    const ask = vi.fn(() => false);
    vi.stubGlobal('confirm', ask);
    expect(confirmClosing(WS)).toBe(true);
    expect(ask).not.toHaveBeenCalled();
  });

  test('an edit in progress: the user decides, and is told which', () => {
    provider([{ describe: 'a.txt', discard: vi.fn() }]);
    const ask = vi.fn(() => false);
    vi.stubGlobal('confirm', ask);
    expect(confirmClosing(WS)).toBe(false);
    expect(ask.mock.calls[0]).toEqual([expect.stringContaining('a.txt')]);
    vi.stubGlobal('confirm', vi.fn(() => true));
    expect(confirmClosing(null)).toBe(true);
  });
});

describe('the panel API', () => {
  function api(invoke: ReturnType<typeof vi.fn>) {
    return createPanelApi(
      { invoke, dispatch: vi.fn(), registerHandler: vi.fn(), registerArgs: vi.fn(), onCommandsChanged: vi.fn(), addDefaultMenuItems: vi.fn() } as never,
      { wsId: WS, panelType: 'metarecord-list', guiServer: 'http://127.0.0.1:7524', visibilityGate: { visible: true, set: vi.fn(), whenVisible: vi.fn() } } as never,
    ).api;
  }

  test('a refused move of the selection writes nothing, the rest is written', async () => {
    provider([{ describe: 'a.txt', discard: vi.fn() }]);
    vi.stubGlobal('confirm', vi.fn(() => false));
    const invoke = vi.fn(async () => null);
    const { workspace } = api(invoke);

    await workspace.set('selected_paths', ['/b']);
    expect(invoke).not.toHaveBeenCalledWith('ws_set_var', expect.anything());

    await tick();
    await workspace.setMany({ selected_paths: ['/b'], 'file-manager:dir': '/x' });
    expect(invoke).toHaveBeenCalledWith('workspace_send', {
      wsId: WS,
      vars: { 'file-manager:dir': '/x' },
    });
  });
});
