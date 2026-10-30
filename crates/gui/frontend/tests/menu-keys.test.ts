// The key a context menu shows beside an entry (doc "Context menus"): one that
// runs that command, from the panel clicked — never a key that would not.

import { describe, expect, test } from 'vitest';
import { menuKey } from '../src/lib/menuKeys';

type Row = { keys: string[]; invocation: string; when?: string; focus?: string };

function table(...rows: Row[]) {
  return rows.map((row) => ({ when: null, focus: null, text_input: false, ...row }));
}

const always = () => true;

describe('menuKey', () => {
  test('spells the key the way the help pages do', () => {
    const bindings = table({ keys: ['ctrl+shift+z'], invocation: 'log:redo' });
    expect(menuKey(bindings, 'log:redo', 'recent', always)).toBe('Ctrl+Shift+z');
  });

  test('is null for a command nothing binds', () => {
    expect(menuKey(table(), 'log:redo', 'recent', always)).toBeNull();
  });

  test('prefers the shortest of several keys', () => {
    const bindings = table(
      { keys: ['p', 'c'], invocation: 'clipboard:copy self' },
      { keys: ['ctrl+c'], invocation: 'clipboard:copy self' },
    );
    expect(menuKey(bindings, 'clipboard:copy self', 'file-manager', always)).toBe('Ctrl+c');
  });

  test('leaves out a key bound in another panel', () => {
    const bindings = table({ keys: ['delete'], invocation: 'item:delete', when: 'viewer' });
    expect(menuKey(bindings, 'item:delete', 'viewer', always)).toBe('Delete');
    expect(menuKey(bindings, 'item:delete', 'recent', always)).toBeNull();
  });

  test('leaves out a key a binding of this panel takes over', () => {
    const bindings = table(
      { keys: ['x'], invocation: 'item:delete' },
      { keys: ['x'], invocation: 'metarecord:row-delete', when: 'metarecord-detail' },
    );
    expect(menuKey(bindings, 'item:delete', 'metarecord-detail', always)).toBeNull();
    expect(menuKey(bindings, 'item:delete', 'recent', always)).toBe('x');
  });

  test('leaves out a command that does not apply in this panel', () => {
    const bindings = table({ keys: ['ctrl+c'], invocation: 'clipboard:copy self' });
    const applies = (invocation: string, panelType: string | null) =>
      invocation !== 'clipboard:copy self' || panelType === 'file-manager';
    expect(menuKey(bindings, 'clipboard:copy self', 'viewer', applies)).toBeNull();
    expect(menuKey(bindings, 'clipboard:copy self', 'file-manager', applies)).toBe('Ctrl+c');
  });

  test('leaves out a key that only fires in a focused widget', () => {
    const bindings = table({ keys: ['enter'], invocation: 'find:next', focus: 'find' });
    expect(menuKey(bindings, 'find:next', 'recent', always)).toBeNull();
  });

  test('a bare name takes any arguments, but the exact invocation comes first', () => {
    const bindings = table(
      { keys: ['e'], invocation: 'item:delete selection' },
      { keys: ['f', 'g'], invocation: 'item:delete' },
    );
    expect(menuKey(bindings, 'item:delete', 'recent', always)).toBe('f g');
    expect(menuKey(bindings, 'metarecord:x', 'recent', always)).toBeNull();
    const withArgs = table({ keys: ['r'], invocation: 'metarecord:reconcile watch' });
    expect(menuKey(withArgs, 'metarecord:reconcile', 'recent', always)).toBe('r');
  });
});
