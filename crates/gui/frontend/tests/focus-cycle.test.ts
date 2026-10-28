// F6 (doc "Keyboard navigation"): `panel:focus cycle` walks the left panel,
// the right panel, then the script's question — when one awaits keys — and
// back to the left panel.

import { describe, expect, test } from 'vitest';
import { cycleStep } from '../src/lib/focusCycle';

describe('panel:focus cycle', () => {
  const two = { split: true, question: false, scriptKeys: false };

  test('left → right → left with no question', () => {
    expect(cycleStep({ ...two, focused: 'left' })).toEqual({ focus: 'right' });
    expect(cycleStep({ ...two, focused: 'right' })).toEqual({ focus: 'left' });
  });

  test('a question awaiting keys comes after the last panel', () => {
    const asking = { ...two, question: true };
    expect(cycleStep({ ...asking, focused: 'left' })).toEqual({ focus: 'right' });
    expect(cycleStep({ ...asking, focused: 'right' })).toEqual({ scriptKeys: true });
    // From the question, back to the left panel, its keys given back.
    expect(cycleStep({ ...asking, scriptKeys: true, focused: 'right' })).toEqual({
      scriptKeys: false,
      focus: 'left',
    });
  });

  test('one panel shown: it and the question, in turn', () => {
    const one = { split: false, question: true, scriptKeys: false, focused: 'left' as const };
    expect(cycleStep(one)).toEqual({ scriptKeys: true });
    expect(cycleStep({ ...one, scriptKeys: true })).toEqual({ scriptKeys: false, focus: 'left' });
    expect(cycleStep({ ...one, question: false })).toEqual({});
  });

  test('the focused slot of a single panel may be the right one', () => {
    const right = { split: false, question: false, scriptKeys: false, focused: 'right' as const };
    expect(cycleStep(right)).toEqual({});
    expect(cycleStep({ ...right, question: true, scriptKeys: true })).toEqual({
      scriptKeys: false,
      focus: 'right',
    });
  });
});
