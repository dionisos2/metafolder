// A script's question (POST /gui/input) and text prompt (POST /gui/prompt)
// belong to the workspace they are asked in: one per workspace, the one on
// screen is shown, and the command input gives the line back to the user while
// none is (doc "Script sessions").

import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async () => null),
  listen: vi.fn(async () => () => {}),
}));

import {
  offerPrompt,
  pickOwned,
  promptRequestState,
  promptShown,
  settlePrompts,
  store,
} from '../src/lib/store.svelte';

describe('promptRequestState', () => {
  it('carries the prompt text, its completions, its workspace and its id', () => {
    expect(
      promptRequestState({
        id: 4,
        prompt: 'Tag: ',
        completions: ['jazz', 'rock'],
        workspace: 'ws-2',
        task: 'script-9',
      }),
    ).toEqual({
      id: 4,
      text: 'Tag: ',
      completions: ['jazz', 'rock'],
      workspaces: ['ws-2'],
      task: 'script-9',
    });
  });

  it('defaults the missing fields: a prompt from outside a script owns nothing', () => {
    expect(promptRequestState({ id: 1, prompt: 'Tag: ' })).toEqual({
      id: 1,
      text: 'Tag: ',
      completions: [],
      workspaces: [],
      task: null,
    });
  });
});

describe('promptShown', () => {
  it('shows a prompt whose workspace is visible', () => {
    expect(promptShown('Tag: ', ['ws-1'], ['ws-1', 'ws-2'])).toBe('Tag: ');
  });

  it('puts it away while its workspace is not on screen', () => {
    expect(promptShown('Tag: ', ['ws-1'], ['ws-3'])).toBeNull();
  });

  it('always shows a prompt owned by nobody', () => {
    expect(promptShown('Tag: ', [], ['ws-3'])).toBe('Tag: ');
  });

  it('is null when no prompt is waiting', () => {
    expect(promptShown(null, [], ['ws-1'])).toBeNull();
  });
});

describe('pickOwned', () => {
  const a = { workspace: 'ws-a' };
  const b = { workspace: 'ws-b' };
  const nobody = { workspace: null };
  const open = ['ws-a', 'ws-b', 'ws-c'];

  it('prefers the focused workspace', () => {
    expect(pickOwned([a, b], 'ws-b', ['ws-a', 'ws-b'], open)).toBe(b);
  });

  it('falls back to the other visible workspace', () => {
    expect(pickOwned([a, b], 'ws-c', ['ws-c', 'ws-a'], open)).toBe(a);
  });

  it('shows nothing of a workspace that is not on screen', () => {
    expect(pickOwned([a, b], 'ws-c', ['ws-c'], open)).toBeNull();
  });

  it('shows a question owned by nobody anywhere, after the visible ones', () => {
    expect(pickOwned([nobody], 'ws-c', ['ws-c'], open)).toBe(nobody);
    expect(pickOwned([nobody, a], 'ws-a', ['ws-a'], open)).toBe(a);
  });

  it('shows a question whose workspace was closed anywhere', () => {
    const gone = { workspace: 'ws-gone' };
    expect(pickOwned([gone], 'ws-a', ['ws-a'], open)).toBe(gone);
  });
});

describe('one prompt per workspace', () => {
  function show(ws: string) {
    store.layout.left = { visible: true, workspace_id: ws, panel_type: 'message' };
    store.layout.right = { visible: false, workspace_id: null, panel_type: null };
    store.layout.focused = 'left';
    settlePrompts();
  }
  function prompt(id: number, ws: string, text: string) {
    return {
      id,
      text,
      completions: [],
      items: [],
      more: false,
      source: null,
      viewIndex: 0,
      workspaces: [ws],
      task: null,
      draft: '',
      resolver: null,
      alternate: null,
    };
  }

  beforeEach(() => {
    store.workspaces = ['ws-a', 'ws-b'].map((id) => ({
      id,
      name: id,
      active_repo: null,
      repo_name: null,
    }));
    store.ui.promptText = null;
    store.ui.promptId = null;
    store.ui.parkedPrompts = [];
    show('ws-a');
  });

  it('switching workspace shows the prompt asked there', () => {
    offerPrompt(prompt(1, 'ws-a', 'A? '));
    offerPrompt(prompt(2, 'ws-b', 'B? '));
    expect(store.ui.promptText).toBe('A? ');
    show('ws-b');
    expect(store.ui.promptText).toBe('B? ');
    expect(store.ui.promptId).toBe(2);
    show('ws-a');
    expect(store.ui.promptText).toBe('A? ');
    expect(store.ui.promptId).toBe(1);
  });

  it('a parked prompt keeps what was typed', () => {
    offerPrompt(prompt(1, 'ws-a', 'A? '));
    offerPrompt(prompt(2, 'ws-b', 'B? '));
    store.ui.promptDraft = 'half an answer';
    show('ws-b');
    show('ws-a');
    expect(store.ui.promptDraft).toBe('half an answer');
  });
});
