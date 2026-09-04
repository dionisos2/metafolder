import { describe, expect, it } from 'vitest';
import { inputWaitAnswer, inputWaitState } from '../src/lib/store.svelte';

// The script input question (POST /gui/input) is derived into its own state,
// shown in a dedicated bar so a status/error message can never hide it.

describe('inputWaitState', () => {
  it('carries the question and accepted keys while active', () => {
    expect(inputWaitState({ active: true, prompt: 'Which action?', temp_keys: ['a', 'r'] })).toEqual(
      { prompt: 'Which action?', keys: ['a', 'r'], workspaces: [] },
    );
  });

  it('uses a generic label when the script gives no question', () => {
    expect(inputWaitState({ active: true, prompt: null, temp_keys: ['y', 'n'] })).toEqual({
      prompt: 'Waiting for input',
      keys: ['y', 'n'],
      workspaces: [],
    });
    // Missing fields default the same way.
    expect(inputWaitState({ active: true })).toEqual({
      prompt: 'Waiting for input',
      keys: [],
      workspaces: [],
    });
  });

  it('is null when no wait is active', () => {
    expect(inputWaitState({ active: false, prompt: 'ignored', temp_keys: ['x'] })).toBeNull();
  });

  it('carries the workspaces the asking script owns', () => {
    expect(
      inputWaitState({ active: true, prompt: '?', temp_keys: ['y'], workspaces: ['ws-1', 'ws-2'] }),
    ).toEqual({ prompt: '?', keys: ['y'], workspaces: ['ws-1', 'ws-2'] });
  });

  it('owns no workspace when the wait comes from outside a script', () => {
    expect(inputWaitState({ active: true, prompt: '?', temp_keys: ['y'] })?.workspaces).toEqual([]);
  });
});

describe('inputWaitAnswer (input wait wins over normal keybindings)', () => {
  const wait = { prompt: 'Which action?', keys: ['y', 'n', 's', 'escape'] };

  it('returns the awaited key a pressed combo answers', () => {
    expect(inputWaitAnswer(wait, 'n')).toBe('n');
    expect(inputWaitAnswer(wait, 'escape')).toBe('escape');
  });

  it('preserves the script\'s original key spelling (case-insensitive match)', () => {
    expect(inputWaitAnswer({ keys: ['Escape'] }, 'escape')).toBe('Escape');
  });

  it('returns null for a key the wait does not await (falls through to bindings)', () => {
    expect(inputWaitAnswer(wait, 'x')).toBeNull();
    expect(inputWaitAnswer(wait, 'ctrl+k')).toBeNull();
  });

  it('returns null when no wait is active or the combo is null', () => {
    expect(inputWaitAnswer(null, 'n')).toBeNull();
    expect(inputWaitAnswer(wait, null)).toBeNull();
  });
});
