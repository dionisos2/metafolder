// Pure logic of the command input: invocation parsing and autocomplete
// filtering (lib/commands.ts). Dispatch itself talks to Tauri and is
// exercised in the running app.

import { describe, expect, test } from 'vitest';
import {
  filterCommands,
  filterCompletions,
  needsMessagePanel,
  parseInvocation,
  resolveSubmission,
  shortcutsFor,
  shouldLogCommand,
} from '../src/lib/commands';
import type { LayoutView } from '../src/lib/types';

describe('parseInvocation', () => {
  test('plain command name', () => {
    expect(parseInvocation('tab:new')).toEqual({ name: 'tab:new', args: [] });
  });

  test('command with parameters', () => {
    expect(parseInvocation('metarecord-list:set-mode grid')).toEqual({
      name: 'metarecord-list:set-mode',
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
    expect(parseInvocation('  tab:goto 3  ')).toEqual({ name: 'tab:goto', args: ['3'] });
  });
});

describe('needsMessagePanel', () => {
  const slot = (visible: boolean, workspace_id: string | null, panel_type: string | null) => ({
    visible,
    workspace_id,
    panel_type,
  });
  const layout = (left: ReturnType<typeof slot>, right: ReturnType<typeof slot>): LayoutView =>
    ({ left, right, focused: 'left' }) as LayoutView;

  test('needed when no slot of the workspace shows message', () => {
    const l = layout(slot(true, 'ws1', 'file'), slot(false, null, null));
    expect(needsMessagePanel(l, 'ws1')).toBe(true);
  });

  test('not needed when the focused slot already shows message', () => {
    const l = layout(slot(true, 'ws1', 'message'), slot(false, null, null));
    expect(needsMessagePanel(l, 'ws1')).toBe(false);
  });

  test('not needed when the other slot already shows message', () => {
    const l = layout(slot(true, 'ws1', 'file'), slot(true, 'ws1', 'message'));
    expect(needsMessagePanel(l, 'ws1')).toBe(false);
  });

  test('a hidden message slot does not count', () => {
    const l = layout(slot(true, 'ws1', 'file'), slot(false, 'ws1', 'message'));
    expect(needsMessagePanel(l, 'ws1')).toBe(true);
  });

  test('a message slot of another workspace does not count', () => {
    const l = layout(slot(true, 'ws1', 'file'), slot(true, 'ws2', 'message'));
    expect(needsMessagePanel(l, 'ws1')).toBe(true);
  });

  test('no focused workspace: never needed', () => {
    const l = layout(slot(false, null, null), slot(false, null, null));
    expect(needsMessagePanel(l, null)).toBe(false);
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
    binding(['ctrl+g'], 'metarecord-list:set-mode grid'),
    binding(['down'], 'metarecord-list:next'),
    binding(['j'], 'metarecord-list:next'),
    binding(['g', 'g'], 'metarecord-list:goto-top'),
  ];

  test('exact invocation match', () => {
    expect(shortcutsFor(table, 'tab:new')).toEqual(['alt+t']);
  });

  test('parameterized invocations count for the bare command', () => {
    expect(shortcutsFor(table, 'metarecord-list:set-mode')).toEqual(['ctrl+g']);
  });

  test('several bindings are all listed', () => {
    expect(shortcutsFor(table, 'metarecord-list:next')).toEqual(['down', 'j']);
  });

  test('sequences are space-joined', () => {
    expect(shortcutsFor(table, 'metarecord-list:goto-top')).toEqual(['g g']);
  });

  test('no binding yields an empty list, not a partial-name match', () => {
    expect(shortcutsFor(table, 'tab:close')).toEqual([]);
    expect(shortcutsFor(table, 'metarecord-list:go')).toEqual([]);
  });
});

describe('shouldLogCommand', () => {
  const commands = [
    { name: 'reconcile:run', log: true },
    { name: 'editing:confirm', log: false },
    { name: 'tab:goto', log: true },
  ];

  test('logs a command whose definition opts in', () => {
    expect(shouldLogCommand(commands, 'reconcile:run')).toBe(true);
  });

  test('does not log a command that opts out', () => {
    expect(shouldLogCommand(commands, 'editing:confirm')).toBe(false);
  });

  test('unknown commands default to logging', () => {
    expect(shouldLogCommand(commands, 'p:never-registered')).toBe(true);
  });
});

describe('filterCommands', () => {
  const all = [
    { name: 'panel:unsplit', label: 'Hide the non-focused panel slot' },
    { name: 'panel:split', label: 'Show the second panel slot' },
    { name: 'tab:close', label: "Close the focused slot's workspace" },
    { name: 'tab:new', label: 'Create a workspace' },
  ];

  test('prefix matches come first, then substring matches', () => {
    const names = filterCommands(all, 'pa').map((c) => c.name);
    expect(names).toEqual(['panel:split', 'panel:unsplit']);

    // Substring matches are alphabetical among themselves ("sp" is a
    // substring of both, a prefix of neither).
    const sp = filterCommands(all, 'sp').map((c) => c.name);
    expect(sp).toEqual(['panel:split', 'panel:unsplit']);
  });

  test('empty filter lists everything', () => {
    expect(filterCommands(all, '').length).toBe(4);
  });

  test('no match yields an empty list', () => {
    expect(filterCommands(all, 'zzz')).toEqual([]);
  });

  test('space-separated terms match fuzzily, in order ("con def" ≈ .*con.*def.*)', () => {
    const names = filterCommands(all, 'pan spl').map((c) => c.name);
    expect(names).toEqual(['panel:split', 'panel:unsplit']);
    // Terms must appear in order, without overlapping.
    expect(filterCommands(all, 'spl pan')).toEqual([]);
    expect(filterCommands(all, 'tab close').map((c) => c.name)).toEqual(['tab:close']);
  });

  test('matching is case-insensitive', () => {
    const names = filterCommands(all, 'PAN').map((c) => c.name);
    expect(names).toEqual(['panel:split', 'panel:unsplit']);
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

  test('space-separated terms match fuzzily, in order', () => {
    expect(filterCompletions(tags, 'ja be')).toEqual(['jazz/bebop']);
    expect(filterCompletions(tags, 'be ja')).toEqual([]);
  });
});

describe('resolveSubmission', () => {
  const sugg = [{ name: 'panel:swap' }, { name: 'panel:split' }];

  test('runs the highlighted suggestion when the list is non-empty', () => {
    expect(resolveSubmission('pan', sugg, 0)).toBe('panel:swap');
    expect(resolveSubmission('pan', sugg, 1)).toBe('panel:split');
  });

  test('clamps an out-of-range selection', () => {
    expect(resolveSubmission('pan', sugg, 9)).toBe('panel:split');
    expect(resolveSubmission('pan', sugg, -1)).toBe('panel:swap');
  });

  test('falls back to the typed text when there is no suggestion', () => {
    expect(resolveSubmission('panel:set-type file', [], 0)).toBe('panel:set-type file');
    expect(resolveSubmission('!ls', [], 0)).toBe('!ls');
  });
});
