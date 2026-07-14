// The shared key matcher (panel-shim/keymatch.js) consumes the compiled
// binding table produced by the Rust engine. Both the Svelte shell and the
// panel shim use it, so key handling behaves identically inside iframes.

import { describe, expect, test } from 'vitest';
import { comboFromEvent, createMatcher } from '../../panel-shim/keymatch.js';

type Binding = {
  keys: string[];
  invocation: string;
  when: string | null;
  text_input: boolean;
  focus: string | null;
};

const b = (
  keys: string[],
  invocation: string,
  when: string | null = null,
  textInput = false,
  focus: string | null = null,
): Binding => ({ keys, invocation, when, text_input: textInput, focus });

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

  test('keeps shift on a letter when another modifier is held', () => {
    // Ctrl+Shift+Z yields key "Z" (length 1); shift must survive so the
    // combo is ctrl+shift+z (redo), not ctrl+z (undo).
    expect(comboFromEvent({ key: 'Z', ctrlKey: true, shiftKey: true })).toBe(
      'ctrl+shift+z',
    );
    // Plain Shift+; keeps shift baked into the resulting ":" character.
    expect(comboFromEvent({ key: ':', shiftKey: true })).toBe(':');
  });

  test('maps special keys to the Rust-side names', () => {
    expect(comboFromEvent({ key: 'Escape' })).toBe('escape');
    expect(comboFromEvent({ key: 'ArrowLeft' })).toBe('left');
    expect(comboFromEvent({ key: ' ' })).toBe('space');
    expect(comboFromEvent({ key: 'Enter' })).toBe('enter');
  });

  test('maps "+" to "plus" (the literal "+" is the combo separator)', () => {
    // "+" cannot be a combo string ("a+b" means a chord); the engine needs a
    // word. Typed with or without shift (shift+= on most layouts) it stays
    // "plus", never "shift+plus".
    expect(comboFromEvent({ key: '+' })).toBe('plus');
    expect(comboFromEvent({ key: '+', shiftKey: true })).toBe('plus');
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

  test('Ctrl+Shift+Z fires redo, not undo (full event path)', () => {
    const matcher = createMatcher([
      b(['ctrl+z'], 'log:undo'),
      b(['ctrl+shift+z'], 'log:redo'),
    ]);
    const undo = comboFromEvent({ key: 'z', ctrlKey: true });
    const redo = comboFromEvent({ key: 'Z', ctrlKey: true, shiftKey: true });
    expect(matcher.feed(undo, noInput)).toEqual({ invocation: 'log:undo' });
    expect(matcher.feed(redo, noInput)).toEqual({ invocation: 'log:redo' });
  });

  test('local binding wins over global for the focused panel type', () => {
    const matcher = createMatcher([
      b(['j'], 'global:j'),
      b(['j'], 'metarecord-list:next', 'metarecord-list'),
    ]);
    expect(matcher.feed('j', { panelType: 'metarecord-list', textInput: false })).toEqual({
      invocation: 'metarecord-list:next',
    });
    expect(matcher.feed('j', { panelType: 'file', textInput: false })).toEqual({
      invocation: 'global:j',
    });
    expect(matcher.feed('j', noInput)).toEqual({ invocation: 'global:j' });
  });

  test('text-input=false bindings are suppressed while typing', () => {
    const matcher = createMatcher([
      b(['j'], 'metarecord-list:next', 'metarecord-list'),
      b(['escape'], 'editing:unfocus', null, true),
    ]);
    const typing = { panelType: 'metarecord-list', textInput: true };
    expect(matcher.feed('j', typing)).toBeNull();
    expect(matcher.feed('escape', typing)).toEqual({ invocation: 'editing:unfocus' });
  });

  test('a focus-scoped binding fires in its widget even while typing', () => {
    const matcher = createMatcher([
      b(['down'], 'metarecord-list:next', 'metarecord-list'), // suppressed while typing
      b(['down'], 'metarecord-list:next', null, false, 'finder'),
    ]);
    expect(
      matcher.feed('down', { panelType: 'metarecord-list', textInput: true, focus: 'finder' }),
    ).toEqual({ invocation: 'metarecord-list:next' });
  });

  test('a focus-scoped binding is inert when its widget is not focused', () => {
    const matcher = createMatcher([b(['ctrl+enter'], 'pick:confirm', null, false, 'finder')]);
    // No focus scope, or a different one: the binding does not fire.
    expect(
      matcher.feed('ctrl+enter', { panelType: 'metarecord-list', textInput: true }),
    ).toBeNull();
    expect(
      matcher.feed('ctrl+enter', { panelType: 'metarecord-list', textInput: true, focus: 'other' }),
    ).toBeNull();
  });

  test('focus-scoped wins over when/global for the same combo', () => {
    const matcher = createMatcher([
      b(['enter'], 'editing:confirm', null, true),
      b(['enter'], 'metarecord-list:open', 'metarecord-list'),
      b(['enter'], 'metarecord-list:apply-finder', null, false, 'finder'),
    ]);
    expect(
      matcher.feed('enter', { panelType: 'metarecord-list', textInput: true, focus: 'finder' }),
    ).toEqual({ invocation: 'metarecord-list:apply-finder' });
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
    const matcher = createMatcher([b(['g', 'g'], 'metarecord-list:goto-top', 'metarecord-list')]);
    const ctx = { panelType: 'metarecord-list', textInput: false };
    expect(matcher.feed('g', ctx)).toMatchObject({ pending: true, prefix: ['g'] });
    expect(matcher.feed('g', ctx)).toEqual({ invocation: 'metarecord-list:goto-top' });
  });

  test('a pending result carries the continuation candidates (hint display)', () => {
    const list = b(['s', 'l'], 'panel:set-type metarecord-list');
    const detail = b(['s', 'd'], 'panel:set-type metarecord-detail');
    const matcher = createMatcher([list, detail, b(['x'], 'cut')]);
    expect(matcher.feed('s', noInput)).toEqual({
      pending: true,
      prefix: ['s'],
      candidates: [list, detail],
    });
  });

  test('an unknown continuation aborts the sequence without firing another binding', () => {
    const matcher = createMatcher([
      b(['g', 'g'], 'goto-top'),
      b(['x'], 'cut'),
    ]);
    expect(matcher.feed('g', noInput)).toMatchObject({ pending: true });
    // 'x' is not a valid continuation of 'g': a combo in progress swallows
    // other keys, so the standalone 'x' binding must NOT fire.
    expect(matcher.feed('x', noInput)).toEqual({ unknown: true, sequence: ['g', 'x'] });
    // The sequence was dropped: 'x' on its own now fires its binding.
    expect(matcher.feed('x', noInput)).toEqual({ invocation: 'cut' });
  });

  test('a pending prefix swallows an unrelated single-key binding (s then t)', () => {
    const setType = b(['s', 'l'], 'panel:set-type metarecord-list');
    const tab = b(['t'], 'tab:new');
    const matcher = createMatcher([setType, tab]);
    expect(matcher.feed('s', noInput)).toMatchObject({ pending: true, prefix: ['s'] });
    // 't' would fire tab:new on its own, but the 's' combo is in progress.
    expect(matcher.feed('t', noInput)).toEqual({ unknown: true, sequence: ['s', 't'] });
  });

  test('escape cancels a pending sequence instead of matching', () => {
    const matcher = createMatcher([
      b(['g', 'g'], 'goto-top'),
      b(['escape'], 'editing:unfocus', null, true),
    ]);
    expect(matcher.feed('g', noInput)).toMatchObject({ pending: true });
    expect(matcher.feed('escape', noInput)).toEqual({ cancelled: true });
    // Without a pending sequence escape matches its own binding.
    expect(matcher.feed('escape', noInput)).toEqual({ invocation: 'editing:unfocus' });
    // The sequence really was dropped: 'g' starts over.
    expect(matcher.feed('g', noInput)).toMatchObject({ pending: true, prefix: ['g'] });
  });

  test('a pending sequence never expires', () => {
    // No clock is consulted at all: a prefix stays pending until a key
    // completes, aborts or cancels it (spec-gui "Keybinding").
    const matcher = createMatcher([b(['g', 'g'], 'goto-top')]);
    expect(matcher.feed('g', noInput)).toMatchObject({ pending: true });
    expect(matcher.feed('g', noInput)).toEqual({ invocation: 'goto-top' });
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
