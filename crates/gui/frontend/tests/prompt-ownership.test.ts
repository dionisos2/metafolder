// A script's text prompt (POST /gui/prompt) belongs to the workspaces the
// asking script owns, like its question bar: the command input shows it only
// while one of them is on screen, and gives the line back to the user
// meanwhile (spec-gui "Ownership of a script's workspaces").

import { describe, expect, it } from 'vitest';
import { promptRequestState, promptShown } from '../src/lib/store.svelte';

describe('promptRequestState', () => {
  it('carries the prompt text, its completions and its owner', () => {
    expect(
      promptRequestState({
        prompt: 'Tag: ',
        completions: ['jazz', 'rock'],
        workspaces: ['ws-1', 'ws-2'],
        task: 'script-9',
      }),
    ).toEqual({
      text: 'Tag: ',
      completions: ['jazz', 'rock'],
      workspaces: ['ws-1', 'ws-2'],
      task: 'script-9',
    });
  });

  it('defaults the missing fields: a prompt from outside a script owns nothing', () => {
    expect(promptRequestState({ prompt: 'Tag: ' })).toEqual({
      text: 'Tag: ',
      completions: [],
      workspaces: [],
      task: null,
    });
  });
});

describe('promptShown', () => {
  it('shows a prompt whose script owns a visible workspace', () => {
    expect(promptShown('Tag: ', ['ws-1'], ['ws-1', 'ws-2'])).toBe('Tag: ');
  });

  it('puts it away while none of the owned workspaces is on screen', () => {
    expect(promptShown('Tag: ', ['ws-1'], ['ws-3'])).toBeNull();
  });

  it('always shows a prompt owned by nobody', () => {
    expect(promptShown('Tag: ', [], ['ws-3'])).toBe('Tag: ');
  });

  it('is null when no prompt is waiting', () => {
    expect(promptShown(null, [], ['ws-1'])).toBeNull();
  });
});
