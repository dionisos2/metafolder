// `command-input:focus <mode> [clear]` (doc "command-input:focus"): the line of
// the mode entered comes back, or is emptied when asked. A question on the line
// keeps its answer.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { flushSync, mount, unmount } from 'svelte';
import CommandInput from '../src/components/CommandInput.svelte';
import { dispatch } from '../src/lib/commands';
import { store } from '../src/lib/store.svelte';

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async () => null),
  listen: vi.fn(async () => () => {}),
}));

describe('command-input:focus', () => {
  let component: ReturnType<typeof mount>;
  let input: HTMLInputElement;

  const type = (text: string) => {
    input.value = text;
    input.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
  };
  const run = async (invocation: string) => {
    await dispatch(invocation);
    flushSync();
  };

  beforeEach(() => {
    // The drafts are per workspace.
    store.layout.left = { ...store.layout.left, workspace_id: 'ws' };
    store.layout.right = { ...store.layout.right, workspace_id: 'ws' };
    const target = document.createElement('div');
    document.body.append(target);
    component = mount(CommandInput, { target });
    flushSync();
    input = target.querySelector('input')!;
  });

  afterEach(() => {
    store.ui.promptText = null;
    void unmount(component);
    document.body.replaceChildren();
  });

  test('plain, it keeps the line', async () => {
    await run('command-input:focus command');
    type('panel:');
    await run('command-input:focus command');
    expect(input.value).toBe('panel:');
    expect(document.activeElement).toBe(input);
  });

  test('clear empties the line and focuses it', async () => {
    await run('command-input:focus command');
    type('panel:');
    input.blur();
    await run('command-input:focus command clear');
    expect(input.value).toBe('');
    expect(document.activeElement).toBe(input);
  });

  test('clear empties the line of the mode entered, not the other', async () => {
    await run('command-input:focus command');
    type('panel:');
    await run('command-input:focus bash');
    type('ls');
    await run('command-input:focus command clear');
    expect(input.value).toBe('');
    await run('command-input:focus bash');
    expect(input.value).toBe('ls');
  });

  test('a question keeps what is answered', async () => {
    await run('command-input:focus command');
    store.ui.promptText = 'Which one?';
    flushSync();
    type('half an answer');
    await run('command-input:focus command clear');
    expect(input.value).toBe('half an answer');
  });

  test('a clear asked during a question is not kept for later', async () => {
    await run('command-input:focus command');
    store.ui.promptText = 'Which one?';
    flushSync();
    await run('command-input:focus command clear');
    store.ui.promptText = null;
    flushSync();
    type('panel:');
    await run('command-input:focus command');
    expect(input.value).toBe('panel:');
  });

  test('refuses another modifier', async () => {
    await run('command-input:focus command');
    type('panel:');
    await run('command-input:focus command nope');
    expect(input.value).toBe('panel:');
  });
});
