import { describe, expect, it } from 'vitest';
import { inputWaitAction, inputWaitAnswer, questionsState } from '../src/lib/store.svelte';

// The script input question (POST /gui/input) is derived into its own state,
// shown in a dedicated bar so a status/error message can never hide it.

describe('questionsState', () => {
  it('carries each question, its keys, its workspace and its id', () => {
    expect(
      questionsState({
        questions: [
          { id: 3, prompt: 'Which action?', keys: ['a', 'r'], workspace: 'ws-2', task: 's-1' },
        ],
      }),
    ).toEqual([
      { id: 3, prompt: 'Which action?', keys: ['a', 'r'], workspace: 'ws-2', task: 's-1' },
    ]);
  });

  it('uses a generic label when the script gives no question', () => {
    expect(questionsState({ questions: [{ id: 1, prompt: null, keys: ['y', 'n'] }] })).toEqual([
      { id: 1, prompt: 'Waiting for input', keys: ['y', 'n'], workspace: null, task: null },
    ]);
  });

  it('is empty when no wait is active', () => {
    expect(questionsState({ questions: [] })).toEqual([]);
    expect(questionsState({})).toEqual([]);
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

  it('kills the asking script on ctrl+escape, for one that ignores the stop', () => {
    expect(inputWaitAction(wait, true, 'ctrl+escape')).toEqual({ kind: 'kill', task: 'script-3' });
    expect(inputWaitAction(wait, false, 'ctrl+escape')).toBeNull();
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

  it('sends "back" on backspace, which no script can await either', () => {
    // The way out of an answer already given. Like escape it is reserved, so a
    // script cannot take the key; unlike escape it does not end the run — the
    // script decides what going back means (undo its last write, re-ask), and
    // one that has no answer for it simply asks again.
    expect(inputWaitAction(wait, true, 'backspace')).toEqual({ kind: 'answer', value: 'back' });
    // It is a question key like any other: disabled with the script keys.
    expect(inputWaitAction(wait, false, 'backspace')).toBeNull();
    // And it means nothing when no question is up.
    expect(inputWaitAction(null, true, 'backspace')).toBeNull();
  });
});
