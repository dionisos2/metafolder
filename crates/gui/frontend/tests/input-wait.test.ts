import { describe, expect, it } from 'vitest';
import { inputWaitAction, inputWaitAnswer, inputWaitState } from '../src/lib/store.svelte';

// The script input question (POST /gui/input) is derived into its own state,
// shown in a dedicated bar so a status/error message can never hide it.

describe('inputWaitState', () => {
  it('carries the question and accepted keys while active', () => {
    expect(inputWaitState({ active: true, prompt: 'Which action?', temp_keys: ['a', 'r'] })).toEqual(
      { prompt: 'Which action?', keys: ['a', 'r'], workspaces: [], task: null },
    );
  });

  it('uses a generic label when the script gives no question', () => {
    expect(inputWaitState({ active: true, prompt: null, temp_keys: ['y', 'n'] })).toEqual({
      prompt: 'Waiting for input',
      keys: ['y', 'n'],
      workspaces: [],
      task: null,
    });
    // Missing fields default the same way.
    expect(inputWaitState({ active: true })).toEqual({
      prompt: 'Waiting for input',
      keys: [],
      workspaces: [],
      task: null,
    });
  });

  it('is null when no wait is active', () => {
    expect(inputWaitState({ active: false, prompt: 'ignored', temp_keys: ['x'] })).toBeNull();
  });

  it('carries the workspaces the asking script owns', () => {
    expect(
      inputWaitState({ active: true, prompt: '?', temp_keys: ['y'], workspaces: ['ws-1', 'ws-2'] }),
    ).toEqual({ prompt: '?', keys: ['y'], workspaces: ['ws-1', 'ws-2'], task: null });
  });

  it('owns no workspace when the wait comes from outside a script', () => {
    expect(inputWaitState({ active: true, prompt: '?', temp_keys: ['y'] })?.workspaces).toEqual([]);
  });

  it('carries the asking script\'s run id, so escape knows what to stop', () => {
    expect(inputWaitState({ active: true, temp_keys: ['y'], task: 'script-3' })?.task).toBe(
      'script-3',
    );
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


describe('inputWaitAction (what a key does while a question is up)', () => {
  const wait = { keys: ['y', 'n'], task: 'script-3' };

  it('answers the script with an awaited key', () => {
    expect(inputWaitAction(wait, true, 'n')).toEqual({ kind: 'answer', value: 'n' });
  });

  it('stops the asking script on escape, which no script can await', () => {
    expect(inputWaitAction(wait, true, 'escape')).toEqual({ kind: 'stop', task: 'script-3' });
    // A wait nobody owns has no script to stop: the question is closed instead.
    expect(inputWaitAction({ keys: ['y'], task: null }, true, 'escape')).toEqual({
      kind: 'stop',
      task: null,
    });
  });

  it('does nothing with the script keys disabled: the GUI bindings take over', () => {
    expect(inputWaitAction(wait, false, 'n')).toBeNull();
    expect(inputWaitAction(wait, false, 'escape')).toBeNull();
  });

  it('falls through for a key the script does not await', () => {
    expect(inputWaitAction(wait, true, 'x')).toBeNull();
    expect(inputWaitAction(null, true, 'y')).toBeNull();
    expect(inputWaitAction(wait, true, null)).toBeNull();
  });
});
