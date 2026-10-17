// The GUI's work context (doc "The GUI's work context"): `context:set` opens
// one on the workspace's repository, `context:unset` closes it, and a
// repository opened with one left open asks — once per session — whether to
// stay in it or to end it.

import { describe, expect, test, vi } from 'vitest';
import { dispatch, offerWorkContexts } from '../src/lib/commands';
import { contextIndicator } from '../src/lib/workContext';
import { store } from '../src/lib/store.svelte';
import type { WorkspaceInfo } from '../src/lib/types';

const { invoked, pending } = vi.hoisted(() => ({
  invoked: [] as { cmd: string; args: unknown }[],
  pending: { value: { state: 'none' } as Record<string, unknown> },
}));

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async (cmd: string, args: unknown) => {
    invoked.push({ cmd, args });
    if (cmd === 'context_pending') return pending.value;
    if (cmd === 'context_set') return { label: 3, name: (args as { name: string }).name };
    return null;
  }),
  listen: vi.fn(async () => () => {}),
}));

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

async function answer(value: string | null): Promise<void> {
  await settle();
  const resolve = store.ui.promptResolver;
  expect(store.ui.promptText).not.toBeNull();
  store.ui.promptText = null;
  store.ui.promptResolver = null;
  resolve?.(value);
  await settle();
}

const calls = (cmd: string) => invoked.filter((c) => c.cmd === cmd).map((c) => c.args);

function workspace(id: string, repo: string | null): WorkspaceInfo {
  return { id, active_repo: repo, repo_name: 'photos' } as unknown as WorkspaceInfo;
}

describe('context:set / context:unset', () => {
  test('set takes the name, then the rest of the line as its description', async () => {
    invoked.length = 0;
    await dispatch('context:set "sorting photos" the 2024 trip', { ws: 'ws-1', slot: 'left' });
    expect(calls('context_set')).toEqual([
      { wsId: 'ws-1', name: 'sorting photos', description: 'the 2024 trip' },
    ]);
  });

  test('typed bare, it asks the name, then the description, which may be left empty', async () => {
    invoked.length = 0;
    const done = dispatch('context:set', { ws: 'ws-1', slot: 'left' });
    await settle();
    expect(store.ui.promptText).toContain('Work context name?');
    await answer('sorting photos');
    expect(store.ui.promptText).toContain('Description?');
    await answer('');
    await done;
    expect(calls('context_set')).toEqual([
      { wsId: 'ws-1', name: 'sorting photos', description: null },
    ]);
  });

  test('unset closes it', async () => {
    invoked.length = 0;
    await dispatch('context:unset', { ws: 'ws-1', slot: 'left' });
    expect(calls('context_unset')).toEqual([{ wsId: 'ws-1' }]);
  });
});

describe('a repository opened with a context left open', () => {
  test('asks whether to stay in it; "end" closes it', async () => {
    invoked.length = 0;
    pending.value = { state: 'open', label: 4, name: 'tagging' };
    const done = offerWorkContexts([workspace('ws-1', 'repo-a')]);
    await settle();
    expect(store.ui.promptText).toContain('"tagging"');
    expect(store.ui.promptCompletions).toEqual(['stay', 'end']);
    await answer('end');
    await done;
    expect(calls('context_unset')).toEqual([{ wsId: 'ws-1' }]);
    expect(calls('context_resume')).toEqual([]);
  });

  test('"stay" resumes it, and the repository is not asked about again', async () => {
    invoked.length = 0;
    pending.value = { state: 'open', label: 5, name: 'sorting' };
    const done = offerWorkContexts([workspace('ws-1', 'repo-b')]);
    await answer('stay');
    await done;
    expect(calls('context_resume')).toEqual([{ wsId: 'ws-1' }]);

    // A second workspace on the same repository: nothing asked.
    await offerWorkContexts([workspace('ws-1', 'repo-b'), workspace('ws-2', 'repo-b')]);
    expect(calls('context_pending')).toHaveLength(1);
  });

  test('escape leaves things as they were: the context stays in force', async () => {
    invoked.length = 0;
    pending.value = { state: 'open', label: 6, name: 'x' };
    const done = offerWorkContexts([workspace('ws-1', 'repo-c')]);
    await answer(null);
    await done;
    expect(calls('context_resume')).toEqual([{ wsId: 'ws-1' }]);
  });

  test('a context whose label is gone is only reported', async () => {
    invoked.length = 0;
    pending.value = { state: 'gone', label: 7, name: 'old' };
    await offerWorkContexts([workspace('ws-1', 'repo-d')]);
    expect(store.ui.promptText).toBeNull();
    expect(calls('context_resume')).toEqual([]);
    expect(calls('context_unset')).toEqual([]);
  });

  test('nothing remembered, nothing asked; a workspace without repository is skipped', async () => {
    invoked.length = 0;
    pending.value = { state: 'none' };
    await offerWorkContexts([workspace('ws-1', 'repo-e'), workspace('ws-2', null)]);
    expect(calls('context_pending')).toEqual([{ wsId: 'ws-1' }]);
    expect(store.ui.promptText).toBeNull();
  });
});

describe('the slot header indicator', () => {
  test('shows the context name, and its label and description on hover', () => {
    expect(contextIndicator(null)).toBeNull();
    const shown = contextIndicator({ label: 4, name: 'tagging', description: 'the 2024 trip' });
    expect(shown?.text).toBe('tagging');
    expect(shown?.title).toContain('work context "tagging" (label 4)');
    expect(shown?.title).toContain('the 2024 trip');
    expect(shown?.title).toContain('context:unset');
    expect(contextIndicator({ label: 5, name: 'x' })?.title).not.toContain('undefined');
  });
});
