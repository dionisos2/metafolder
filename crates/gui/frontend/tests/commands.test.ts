// Pure logic of the command input: invocation parsing and autocomplete
// filtering (lib/commands.ts). Dispatch itself talks to Tauri and is
// exercised in the running app.

import { describe, expect, test } from 'vitest';
import {
  commonPrefix,
  filterCommands,
  filterCompletions,
  gotoIndex,
  parseInvocation,
  shortcutsFor,
} from '../src/lib/commands';

describe('parseInvocation', () => {
  test('plain command name', () => {
    expect(parseInvocation('tab:new')).toEqual({ name: 'tab:new', args: [] });
  });

  test('command with parameters', () => {
    expect(parseInvocation('entry-list:set-mode grid')).toEqual({
      name: 'entry-list:set-mode',
      args: ['grid'],
    });
    expect(parseInvocation('answer:send left')).toEqual({
      name: 'answer:send',
      args: ['left'],
    });
  });

  test('double-quoted parameters keep spaces', () => {
    expect(parseInvocation('tab:rename "My music workspace"')).toEqual({
      name: 'tab:rename',
      args: ['My music workspace'],
    });
  });

  test('shell invocations start with !', () => {
    expect(parseInvocation('!ls -la /tmp')).toEqual({ shell: 'ls -la /tmp' });
    expect(parseInvocation('!  echo hi ')).toEqual({ shell: 'echo hi' });
  });

  test('blank input parses to null', () => {
    expect(parseInvocation('')).toBeNull();
    expect(parseInvocation('   ')).toBeNull();
    expect(parseInvocation('!')).toBeNull();
  });

  test('extra whitespace is tolerated', () => {
    expect(parseInvocation('  tab:goto-3  ')).toEqual({ name: 'tab:goto-3', args: [] });
  });
});

describe('gotoIndex', () => {
  test('extracts N from tab:goto-N', () => {
    expect(gotoIndex('tab:goto-3')).toBe(3);
    expect(gotoIndex('tab:goto-12')).toBe(12);
  });

  test('returns null for other commands', () => {
    expect(gotoIndex('tab:goto-')).toBeNull();
    expect(gotoIndex('tab:new')).toBeNull();
  });
});

describe('commonPrefix', () => {
  test('returns the longest shared prefix', () => {
    expect(commonPrefix(['panel:close', 'panel:split', 'panel:focus-next'])).toBe('panel:');
    expect(commonPrefix(['tab:new', 'tab:next'])).toBe('tab:ne');
  });

  test('single name is its own prefix', () => {
    expect(commonPrefix(['quit'])).toBe('quit');
  });

  test('no shared prefix yields the empty string', () => {
    expect(commonPrefix(['panel:close', 'tab:close'])).toBe('');
    expect(commonPrefix([])).toBe('');
  });
});

describe('shortcutsFor', () => {
  const binding = (keys: string[], invocation: string) => ({
    keys,
    invocation,
    when: null,
    text_input: false,
  });
  const table = [
    binding(['alt+t'], 'tab:new'),
    binding(['ctrl+g'], 'entry-list:set-mode grid'),
    binding(['down'], 'entry-list:next'),
    binding(['j'], 'entry-list:next'),
    binding(['g', 'g'], 'entry-list:goto-top'),
  ];

  test('exact invocation match', () => {
    expect(shortcutsFor(table, 'tab:new')).toEqual(['alt+t']);
  });

  test('parameterized invocations count for the bare command', () => {
    expect(shortcutsFor(table, 'entry-list:set-mode')).toEqual(['ctrl+g']);
  });

  test('several bindings are all listed', () => {
    expect(shortcutsFor(table, 'entry-list:next')).toEqual(['down', 'j']);
  });

  test('sequences are space-joined', () => {
    expect(shortcutsFor(table, 'entry-list:goto-top')).toEqual(['g g']);
  });

  test('no binding yields an empty list, not a partial-name match', () => {
    expect(shortcutsFor(table, 'tab:close')).toEqual([]);
    expect(shortcutsFor(table, 'entry-list:go')).toEqual([]);
  });
});

describe('filterCommands', () => {
  const all = [
    { name: 'panel:close', label: 'Hide the non-focused panel slot' },
    { name: 'panel:split', label: 'Show the second panel slot' },
    { name: 'tab:close', label: "Close the focused slot's workspace" },
    { name: 'tab:new', label: 'Create a workspace' },
  ];

  test('prefix matches come first, then substring matches', () => {
    const names = filterCommands(all, 'pa').map((c) => c.name);
    expect(names).toEqual(['panel:close', 'panel:split']);

    // Substring matches are alphabetical among themselves.
    const close = filterCommands(all, 'close').map((c) => c.name);
    expect(close).toEqual(['panel:close', 'tab:close']);
  });

  test('empty filter lists everything', () => {
    expect(filterCommands(all, '').length).toBe(4);
  });

  test('no match yields an empty list', () => {
    expect(filterCommands(all, 'zzz')).toEqual([]);
  });
});

describe('filterCompletions', () => {
  const tags = ['rock', 'jazz', 'jazz/bebop', 'classical'];

  test('prefix matches come first, then substring matches', () => {
    expect(filterCompletions(tags, 'jazz')).toEqual(['jazz', 'jazz/bebop']);
    expect(filterCompletions(tags, 'bop')).toEqual(['jazz/bebop']);
  });

  test('empty draft lists every completion, sorted', () => {
    expect(filterCompletions(tags, '')).toEqual(['classical', 'jazz', 'jazz/bebop', 'rock']);
  });

  test('no match yields an empty list', () => {
    expect(filterCompletions(tags, 'zzz')).toEqual([]);
  });
});
