// The shared key matcher (panel-shim/keymatch.js) consumes the compiled
// binding table produced by the Rust engine. Both the Svelte shell and the
// panel shim use it, so key handling behaves identically inside iframes.

import { describe, expect, test } from 'vitest';
// @ts-expect-error plain-JS module shared with the panel shim
import { comboFromEvent, createMatcher } from '../../panel-shim/keymatch.js';

type Binding = {
  keys: string[];
  invocation: string;
  when: string | null;
  text_input: boolean;
};

const b = (
  keys: string[],
  invocation: string,
  when: string | null = null,
  textInput = false,
): Binding => ({ keys, invocation, when, text_input: textInput });

const noInput = { panelType: null as string | null, textInput: false };

describe('comboFromEvent', () => {
  test('normalizes printable keys without shift', () => {
    expect(comboFromEvent({ key: 'K', ctrlKey: true })).toBe('ctrl+k');
    expect(comboFromEvent({ key: ':' })).toBe(':');
    expect(comboFromEvent({ key: 'j' })).toBe('j');
  });

  test('orders modifiers ctrl, alt, shift, meta', () => {
    expect(
      comboFromEvent({ key: 'F1', shiftKey: true, altKey: true, ctrlKey: true }),
    ).toBe('ctrl+alt+shift+f1');
  });

  test('maps special keys to the Rust-side names', () => {
    expect(comboFromEvent({ key: 'Escape' })).toBe('escape');
    expect(comboFromEvent({ key: 'ArrowLeft' })).toBe('left');
    expect(comboFromEvent({ key: ' ' })).toBe('space');
    expect(comboFromEvent({ key: 'Enter' })).toBe('enter');
  });

  test('returns null for modifier-only events', () => {
    expect(comboFromEvent({ key: 'Control', ctrlKey: true })).toBeNull();
    expect(comboFromEvent({ key: 'Shift', shiftKey: true })).toBeNull();
  });
});

describe('createMatcher', () => {
  test('matches a simple global binding', () => {
    const matcher = createMatcher([b(['ctrl+t'], 'tab:new')]);
    expect(matcher.feed('ctrl+t', noInput)).toEqual({ invocation: 'tab:new' });
    expect(matcher.feed('ctrl+x', noInput)).toBeNull();
  });

  test('local binding wins over global for the focused panel type', () => {
    const matcher = createMatcher([
      b(['j'], 'global:j'),
      b(['j'], 'record-list:next', 'record-list'),
    ]);
    expect(matcher.feed('j', { panelType: 'record-list', textInput: false })).toEqual({
      invocation: 'record-list:next',
    });
    expect(matcher.feed('j', { panelType: 'file', textInput: false })).toEqual({
      invocation: 'global:j',
    });
    expect(matcher.feed('j', noInput)).toEqual({ invocation: 'global:j' });
  });

  test('text-input=false bindings are suppressed while typing', () => {
    const matcher = createMatcher([
      b(['j'], 'record-list:next', 'record-list'),
      b(['escape'], 'editing:unfocus', null, true),
    ]);
    const typing = { panelType: 'record-list', textInput: true };
    expect(matcher.feed('j', typing)).toBeNull();
    expect(matcher.feed('escape', typing)).toEqual({ invocation: 'editing:unfocus' });
  });

  test('strict binding wins over text-input=true when both would fire', () => {
    const matcher = createMatcher([
      b(['ctrl+a'], 'permissive:a', null, true),
      b(['ctrl+a'], 'strict:a', null, false),
    ]);
    expect(matcher.feed('ctrl+a', noInput)).toEqual({ invocation: 'strict:a' });
    // While typing only the permissive one is eligible.
    expect(matcher.feed('ctrl+a', { panelType: null, textInput: true })).toEqual({
      invocation: 'permissive:a',
    });
  });

  test('two-key sequences report pending then fire', () => {
    const matcher = createMatcher([b(['g', 'g'], 'record-list:goto-top', 'record-list')]);
    const ctx = { panelType: 'record-list', textInput: false };
    expect(matcher.feed('g', ctx)).toEqual({ pending: true });
    expect(matcher.feed('g', ctx)).toEqual({ invocation: 'record-list:goto-top' });
  });

  test('an interrupted sequence falls back to single-key matching', () => {
    const matcher = createMatcher([
      b(['g', 'g'], 'goto-top'),
      b(['x'], 'cut'),
    ]);
    expect(matcher.feed('g', noInput)).toEqual({ pending: true });
    // 'x' aborts the sequence but still matches its own binding.
    expect(matcher.feed('x', noInput)).toEqual({ invocation: 'cut' });
  });

  test('sequences expire after the timeout', () => {
    let clock = 0;
    const matcher = createMatcher([b(['g', 'g'], 'goto-top')], {
      timeoutMs: 1000,
      now: () => clock,
    });
    expect(matcher.feed('g', noInput)).toEqual({ pending: true });
    clock = 2000;
    // Too late: the buffer reset, this 'g' starts a new sequence.
    expect(matcher.feed('g', noInput)).toEqual({ pending: true });
  });

  test('setBindings replaces the table', () => {
    const matcher = createMatcher([b(['a'], 'old')]);
    matcher.setBindings([b(['a'], 'new')]);
    expect(matcher.feed('a', noInput)).toEqual({ invocation: 'new' });
  });

  test('exact match beats waiting for a longer sequence', () => {
    // When a key both completes a binding and prefixes a longer one, the
    // exact match fires immediately (simple, predictable rule).
    const matcher = createMatcher([b(['g'], 'single-g'), b(['g', 'g'], 'double-g')]);
    expect(matcher.feed('g', noInput)).toEqual({ invocation: 'single-g' });
  });
});
