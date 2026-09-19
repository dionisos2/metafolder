// Key hints in the help pages (spec-gui "Help"): a page names the *command*
// and the panel fills in the key that is actually bound to it, so the help
// cannot drift from keybindings.toml — neither when a shipped default changes
// nor when the user rebinds something.

import { describe, expect, test } from 'vitest';
import { applyKeyHints, bindingMatches, formatCombo, keysFor } from '../../panel-shim/keyhints.js';

type Binding = { keys: string[]; invocation: string; when?: string | null };

const bind = (keys: string[], invocation: string, when: string | null = null): Binding => ({
  keys,
  invocation,
  when,
});

describe('formatCombo', () => {
  test('a bare letter is left as typed', () => {
    expect(formatCombo(['f'])).toBe('f');
  });

  test('modifiers are capitalised, the key kept', () => {
    expect(formatCombo(['ctrl+f'])).toBe('Ctrl+f');
    expect(formatCombo(['ctrl+shift+z'])).toBe('Ctrl+Shift+z');
  });

  test('named keys are spelled the way the help pages spell them', () => {
    expect(formatCombo(['enter'])).toBe('Enter');
    expect(formatCombo(['shift+enter'])).toBe('Shift+Enter');
    expect(formatCombo(['escape'])).toBe('Escape');
    expect(formatCombo(['backspace'])).toBe('Backspace');
    expect(formatCombo(['f2'])).toBe('F2');
  });

  test('the arrows are arrows', () => {
    expect(formatCombo(['down'])).toBe('↓');
    expect(formatCombo(['shift+up'])).toBe('Shift+↑');
    expect(formatCombo(['left'])).toBe('←');
    expect(formatCombo(['right'])).toBe('→');
  });

  test('the punctuation keys read as themselves', () => {
    // "+" is spelled "plus" in a combo (it is the chord separator).
    expect(formatCombo(['plus'])).toBe('+');
    expect(formatCombo(['/'])).toBe('/');
    expect(formatCombo(['ctrl+/'])).toBe('Ctrl+/');
    expect(formatCombo(['space'])).toBe('Space');
  });

  test('a sequence is its combos in order', () => {
    expect(formatCombo(['t', 'l'])).toBe('t l');
    expect(formatCombo(['m', 'b', 's'])).toBe('m b s');
  });
});

describe('bindingMatches', () => {
  test('an exact invocation matches', () => {
    expect(bindingMatches(bind(['f'], 'treeref:find'), 'treeref:find')).toBe(true);
  });

  test('a parameterised invocation matches its bare command name', () => {
    expect(bindingMatches(bind(['t', 'l'], 'panel:set type metarecord-list'), 'panel:set type')).toBe(
      true,
    );
  });

  test('a fuller query matches only that exact invocation', () => {
    const b = bind(['t', 'l'], 'panel:set type metarecord-list');
    expect(bindingMatches(b, 'panel:set type metarecord-list')).toBe(true);
    expect(bindingMatches(b, 'panel:set type treeref')).toBe(false);
  });

  test('a command name is not a prefix of another command', () => {
    expect(bindingMatches(bind(['f'], 'treeref:find-next'), 'treeref:find')).toBe(false);
  });
});

describe('keysFor', () => {
  const table = [
    bind(['f'], 'metarecord-list:clear finder', 'metarecord-list'),
    bind(['f'], 'file-manager:find', 'file-manager'),
    bind(['f'], 'treeref:find', 'treeref'),
    bind(['/'], 'metarecord-list:focus finder', 'metarecord-list'),
    bind(['e', 'f'], 'metarecord-list:focus finder', 'metarecord-list'),
    bind(['ctrl+f'], 'find:open'),
  ];

  test('every combo bound to the command, in table order', () => {
    expect(keysFor(table, 'metarecord-list:focus finder')).toEqual(['/', 'e f']);
  });

  test('several commands are asked at once, their keys deduplicated', () => {
    // The navigation rows of the keybindings page mean "in any of these
    // panels", and all three panels share the key.
    expect(keysFor(table, 'file-manager:find, treeref:find')).toEqual(['f']);
  });

  test('an unbound command has no keys', () => {
    expect(keysFor(table, 'treeref:root')).toEqual([]);
  });
});

describe('applyKeyHints', () => {
  const table = [bind(['f'], 'treeref:find', 'treeref'), bind(['ctrl+f'], 'find:open')];

  function page(html: string) {
    const host = document.createElement('div');
    host.innerHTML = html;
    return host;
  }

  test('the shipped fallback is replaced by the bound key, in a <kbd>', () => {
    const host = page('<span data-mf-key="treeref:find">x</span>');
    applyKeyHints(host, table);
    expect(host.textContent).toBe('f');
    expect(host.querySelector('span > kbd')?.textContent).toBe('f');
  });

  test('several keys are one <kbd> each', () => {
    const host = page('<span data-mf-key="treeref:find, find:open">?</span>');
    applyKeyHints(host, table);
    expect(host.textContent).toBe('f or Ctrl+f');
    expect([...host.querySelectorAll('kbd')].map((k) => k.textContent)).toEqual(['f', 'Ctrl+f']);
  });

  test('an unbound command says so rather than showing a stale key', () => {
    const host = page('<span data-mf-key="treeref:root">r</span>');
    applyKeyHints(host, table);
    expect(host.textContent).toBe('unbound');
    expect(host.querySelector('span')!.classList.contains('unbound')).toBe(true);
  });

  test('every tagged element is filled, and untagged markup is untouched', () => {
    const host = page(
      '<p><span data-mf-key="treeref:find">x</span> and <code>mfr_path</code> ' +
        'and <span data-mf-key="find:open">y</span></p>',
    );
    applyKeyHints(host, table);
    expect(host.textContent).toBe('f and mfr_path and Ctrl+f');
  });

  test('applying twice is idempotent (a page can be re-rendered)', () => {
    const host = page('<span data-mf-key="treeref:find">x</span>');
    applyKeyHints(host, table);
    applyKeyHints(host, table);
    expect(host.textContent).toBe('f');
    expect(host.querySelectorAll('kbd')).toHaveLength(1);
  });

  test('a command bound again after being unbound loses the unbound mark', () => {
    const host = page('<span data-mf-key="treeref:root">r</span>');
    applyKeyHints(host, table);
    applyKeyHints(host, [...table, bind(['g'], 'treeref:root', 'treeref')]);
    expect(host.textContent).toBe('g');
    expect(host.querySelector('span')!.classList.contains('unbound')).toBe(false);
  });
});
