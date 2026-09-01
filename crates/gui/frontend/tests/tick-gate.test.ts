// Focus-tick gate: the shell signals "focus the command input" by bumping a
// counter in the (module-level) store. A component reading it must react to a
// *change*, never to the value it finds when it is created — CommandInput is
// destroyed and re-created every time the window leaves fullscreen, and firing
// on the value already there popped the autocomplete list open on its own.

import { describe, expect, test } from 'vitest';
import { createTickGate } from '../src/lib/tick';

describe('createTickGate', () => {
  test('does not fire on the value it was created with', () => {
    expect(createTickGate(0)(0)).toBe(false);
    // The regression: a component created long after the first bump.
    expect(createTickGate(7)(7)).toBe(false);
  });

  test('fires once per change', () => {
    const gate = createTickGate(7);
    expect(gate(8)).toBe(true);
    expect(gate(8)).toBe(false);
    expect(gate(9)).toBe(true);
  });

  test('a reset counter is a change like any other', () => {
    const gate = createTickGate(7);
    expect(gate(0)).toBe(true);
    expect(gate(0)).toBe(false);
  });
});
