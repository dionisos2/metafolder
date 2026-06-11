// Pure logic of the command input: invocation parsing and autocomplete
// filtering (lib/commands.ts). Dispatch itself talks to Tauri and is
// exercised in the running app.

import { describe, expect, test } from 'vitest';
import { filterCommands, gotoIndex, parseInvocation } from '../src/lib/commands';

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
