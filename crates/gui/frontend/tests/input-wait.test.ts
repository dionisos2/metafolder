import { describe, expect, it } from 'vitest';
import { inputWaitState } from '../src/lib/store.svelte';

// The script input question (POST /gui/input) is derived into its own state,
// shown in a dedicated bar so a status/error message can never hide it.

describe('inputWaitState', () => {
  it('carries the question and accepted keys while active', () => {
    expect(inputWaitState({ active: true, prompt: 'Which action?', temp_keys: ['a', 'r'] })).toEqual(
      { prompt: 'Which action?', keys: ['a', 'r'] },
    );
  });

  it('uses a generic label when the script gives no question', () => {
    expect(inputWaitState({ active: true, prompt: null, temp_keys: ['y', 'n'] })).toEqual({
      prompt: 'Waiting for input',
      keys: ['y', 'n'],
    });
    // Missing fields default the same way.
    expect(inputWaitState({ active: true })).toEqual({ prompt: 'Waiting for input', keys: [] });
  });

  it('is null when no wait is active', () => {
    expect(inputWaitState({ active: false, prompt: 'ignored', temp_keys: ['x'] })).toBeNull();
  });
});
