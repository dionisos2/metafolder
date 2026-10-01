// The shell builtins declare their arguments like every other command (doc
// "Interactive command arguments"), so an invocation that merely omits one is collected in the
// minibuffer — with its completions — instead of reaching the handler and
// failing there. `panel:set` typed bare asks which setting, then which panel
// type; `panel:set type` supplies the first and asks only the second.

import { describe, expect, test, vi } from 'vitest';
import {
  type ArgPromptRequest,
  argSpecFor,
  collectArgs,
  dispatch,
  promptsForInput,
} from '../src/lib/commands';
import { store } from '../src/lib/store.svelte';

const { invoked } = vi.hoisted(() => ({ invoked: [] as { cmd: string; args: unknown }[] }));

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async (cmd: string, args: unknown) => {
    invoked.push({ cmd, args });
    return null;
  }),
  listen: vi.fn(async () => () => {}),
}));

/** Lets the collection's pending awaits settle so the next prompt is on. */
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

/** Answers the prompt currently in the minibuffer, exactly as the command
 *  input does: release the input, then hand the value (or null for Escape) to
 *  the collector. */
async function answer(value: string | null): Promise<void> {
  await settle();
  const resolve = store.ui.promptResolver;
  expect(store.ui.promptText).not.toBeNull();
  expect(resolve).not.toBeNull();
  store.ui.promptText = null;
  store.ui.promptResolver = null;
  resolve?.(value);
  await settle();
}

describe('panel:set', () => {
  test('asks its setting, then the panel type, and only then acts', async () => {
    store.panelTypes = ['file', 'log'];
    const done = dispatch('panel:set');

    await settle();
    expect(store.ui.promptText).toBe('Which setting? (type)');
    expect(store.ui.promptCompletions).toEqual(['type']);
    await answer('type');

    expect(store.ui.promptText).toBe('Which panel type?');
    expect(store.ui.promptCompletions).toEqual(['file', 'log']);
    await answer('file');

    expect(await done).toEqual({ ok: true });
    expect(invoked).toContainEqual({
      cmd: 'panel_set_type',
      args: { slot: 'left', panelType: 'file' },
    });
  });

  test('a supplied argument is not asked again, the next one still is', async () => {
    store.panelTypes = ['file', 'log'];
    const done = dispatch('panel:set type');

    await settle();
    expect(store.ui.promptText).toBe('Which panel type?');
    await answer('log');

    expect(await done).toEqual({ ok: true });
    expect(invoked).toContainEqual({
      cmd: 'panel_set_type',
      args: { slot: 'left', panelType: 'log' },
    });
  });

  test('a complete invocation runs without opening the minibuffer', async () => {
    expect(await dispatch('panel:set type file')).toEqual({ ok: true });
    expect(store.ui.promptText).toBeNull();
    expect(invoked).toContainEqual({
      cmd: 'panel_set_type',
      args: { slot: 'left', panelType: 'file' },
    });
  });

  test('the value is asked only for a setting that takes one', async () => {
    // `when` drops the value of an unknown setting — naming it is the
    // handler's job — and an argument supplied inline is not asked again.
    const requests: ArgPromptRequest[] = [];
    const collected = await collectArgs(argSpecFor('panel:set')!, ['nonsense'], async (request) => {
      requests.push(request);
      return 'x';
    });
    expect(requests).toEqual([]);
    expect(collected).toEqual(['nonsense']);
  });

  test('escape abandons the whole invocation, quietly', async () => {
    const done = dispatch('panel:set');
    await settle();
    await answer(null);
    expect(await done).toEqual({ ok: true });
    expect(store.ui.promptText).toBeNull();
  });
});

describe('the other builtins that take arguments', () => {
  test('panel:reveal asks the panel type and completes over the installed ones', async () => {
    // Reveal shows the type for the *same workspace* in the other slot, so it
    // needs one on screen.
    store.panelTypes = ['file', 'help'];
    store.workspaces = [{ id: 'ws-1', name: 'music', active_repo: null, repo_name: null }];
    store.layout.left = { visible: true, workspace_id: 'ws-1', panel_type: 'message' };
    const done = dispatch('panel:reveal');

    await settle();
    expect(store.ui.promptText).toBe('Show which panel type?');
    expect(store.ui.promptCompletions).toEqual(['file', 'help']);
    await answer('help');

    expect(await done).toEqual({ ok: true });
    expect(invoked).toContainEqual({
      cmd: 'panel_set_type',
      args: { slot: 'right', panelType: 'help' },
    });
    store.layout.left = { visible: false, workspace_id: null, panel_type: null };
    store.workspaces = [];
  });

  test('the one-of-these builtins ask, with their choices in the prompt', async () => {
    for (const [invocation, prompt, reply] of [
      ['panel:toggle', 'Which flag? (split / fullscreen)', 'fullscreen'],
      ['panel:focus', 'Focus which slot? (next / left / right)', 'right'],
      ['editing:goto', 'Move the cursor where? (line-start / line-end)', 'line-end'],
      ['command-input:focus', 'Which mode? (command / bash)', 'bash'],
      ['mf:duplicate', 'Which operation? (scan)', 'scan'],
    ] as const) {
      const done = dispatch(invocation);
      await settle();
      expect(store.ui.promptText).toBe(prompt);
      await answer(reply);
      expect(await done).toEqual({ ok: true });
    }
  });

  test('workspace:goto asks which workspace and completes over them', async () => {
    store.workspaces = [
      { id: 'ws-1', name: 'music', active_repo: null, repo_name: null },
      { id: 'ws-2', name: 'photos', active_repo: null, repo_name: null },
    ];
    const done = dispatch('workspace:goto');

    await settle();
    expect(store.ui.promptText).toBe('Go to which workspace?');
    expect(store.ui.promptCompletions).toEqual(['1', '2']);
    await answer('2');

    expect(await done).toEqual({ ok: true });
    expect(invoked).toContainEqual({ cmd: 'workspace_goto', args: { n: 2 } });
  });

  test('workspace:rename asks for the name, pre-filled with the current one', async () => {
    store.workspaces = [{ id: 'ws-1', name: 'music', active_repo: null, repo_name: null }];
    store.layout.left = { visible: true, workspace_id: 'ws-1', panel_type: 'message' };
    const done = dispatch('workspace:rename');

    await settle();
    expect(store.ui.promptText).toBe('Rename the workspace to:');
    expect(store.ui.promptDraft).toBe('music');
    await answer('photos');

    expect(await done).toEqual({ ok: true });
    expect(invoked).toContainEqual({
      cmd: 'workspace_rename',
      args: { wsId: 'ws-1', name: 'photos' },
    });
    store.layout.left = { visible: false, workspace_id: null, panel_type: null };
    store.workspaces = [];
  });

  test('daemon:set asks the setting and the URL, pre-filled with the current one', async () => {
    store.daemonUrl = 'http://127.0.0.1:7523';
    const done = dispatch('daemon:set');

    await settle();
    expect(store.ui.promptText).toBe('Which setting? (url)');
    await answer('url');

    expect(store.ui.promptText).toBe('New daemon URL:');
    expect(store.ui.promptDraft).toBe('http://127.0.0.1:7523');
    await answer('http://127.0.0.1:9999');

    expect(await done).toEqual({ ok: true });
    expect(invoked).toContainEqual({
      cmd: 'daemon_set_url',
      args: { url: 'http://127.0.0.1:9999' },
    });
  });

  test('daemon:set with its arguments inline runs at once', async () => {
    // The documented invocation spells the setting out; it used to be read as
    // a command *name* of "daemon:set url" and reach no handler at all.
    expect(await dispatch('daemon:set url http://127.0.0.1:4242')).toEqual({ ok: true });
    expect(store.ui.promptText).toBeNull();
    expect(invoked).toContainEqual({
      cmd: 'daemon_set_url',
      args: { url: 'http://127.0.0.1:4242' },
    });
  });

  test('answer:send completes over the keys its question awaits', async () => {
    store.ui.inputWait = { prompt: 'Delete it?', keys: ['yes', 'no'], workspaces: [], task: null };
    const done = dispatch('answer:send');

    await settle();
    expect(store.ui.promptText).toBe('Answer with:');
    expect(store.ui.promptCompletions).toEqual(['yes', 'no']);
    await answer('yes');

    expect(await done).toEqual({ ok: true });
    expect(invoked).toContainEqual({ cmd: 'answer_send', args: { value: 'yes' } });
    store.ui.inputWait = null;
  });
});

describe('an optional trailing argument is never asked for', () => {
  test('the builtins with a documented bare form act immediately', async () => {
    for (const invocation of ['workspace:next', 'workspace:prev', 'workspace:new', 'script:stop']) {
      const done = dispatch(invocation);
      await settle();
      expect(store.ui.promptText).toBeNull();
      expect(await done).toEqual({ ok: true });
    }
  });

  test('and the "…" marker follows: only a missing required argument earns it', () => {
    expect(promptsForInput('panel:set')).toBe(true);
    expect(promptsForInput('panel:set type')).toBe(true);
    expect(promptsForInput('panel:set type file')).toBe(false);
    expect(promptsForInput('workspace:rename')).toBe(true);
    expect(promptsForInput('workspace:rename music')).toBe(false);
    expect(promptsForInput('workspace:next')).toBe(false);
    expect(promptsForInput('workspace:next slot')).toBe(false);
    expect(promptsForInput('find:open')).toBe(false);
    expect(promptsForInput('help:open')).toBe(false);
    expect(promptsForInput('script:stop')).toBe(false);
  });
});
