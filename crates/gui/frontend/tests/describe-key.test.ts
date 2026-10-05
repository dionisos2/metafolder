// `help:key` — the Emacs `C-h k` (doc "In-app help"): the shell swallows the next
// key sequence and reports the command it runs instead of running it. The step
// function is pure: given the combos described so far and one more combo, it
// either waits for the rest of the sequence, produces the report, or cancels on
// escape. The lookup happens in the context the key arrived in (focused panel
// type, focus scope, typing or not), so the answer is what the key *would do
// now* — and a combo bound only in other contexts says so instead of reading as
// simply unbound.

import { describe, expect, test } from 'vitest';
import { describeKeyStep } from '../src/lib/describeKey';
import type { Binding } from '../src/lib/types';

const b = (
  keys: string[],
  invocation: string,
  when: string | null = null,
  textInput = false,
  focus: string | null = null,
): Binding => ({ keys, invocation, when, text_input: textInput, focus });

const table: Binding[] = [
  b(['f'], 'file-manager:find', 'file-manager'),
  b(['f'], 'treeref:find', 'treeref'),
  b(['f'], 'trash:find', 'trash'),
  b(['f'], 'recent:find', 'recent'),
  b(['t', 'l'], 'panel:open here metarecord-list'),
  b(['t', 'd'], 'panel:open here metarecord-detail'),
  b(['ctrl+x'], '!echo hi'),
  b(['z'], 'panel:toggle fullscreen'),
  b(['ctrl+enter'], 'metarecord-list:apply finder', null, false, 'finder'),
  b(['ctrl+g'], 'metarecord-list:toggle normal', 'metarecord-list', false, 'columns'),
  b(['e'], 'global:one'),
  b(['e'], 'panel:one', 'metarecord-list'),
];

const labels: Record<string, string> = {
  'file-manager:find': 'Jump to an entry of the displayed directory by name',
  'panel:open': 'Open a panel type here, in the other slot, or in a new workspace',
};

const seen: string[] = [];
const labelOf = (name: string): string => {
  seen.push(name);
  return labels[name] ?? '';
};

const inList = { panelType: 'metarecord-list', textInput: false };
const inFiles = { panelType: 'file-manager', textInput: false };

describe('describeKeyStep — what a key runs', () => {
  test('a key reports its command, with the command description', () => {
    expect(describeKeyStep([], 'f', table, inFiles, labelOf)).toEqual({
      reported: true,
      message: 'f runs file-manager:find — Jump to an entry of the displayed directory by name',
    });
  });

  test('the description comes from the command name, not its arguments', () => {
    seen.length = 0;
    expect(describeKeyStep([], 't', table, inList, labelOf)).toHaveProperty('pending');
    const step = describeKeyStep(['t'], 'l', table, inList, labelOf);
    expect(step).toEqual({
      reported: true,
      message: 't l runs panel:open here metarecord-list — Open a panel type here, in the other slot, or in a new workspace',
    });
    expect(seen).toContain('panel:open');
  });

  test('a command with no description reads without the suffix', () => {
    expect(describeKeyStep([], 'ctrl+x', table, inList, labelOf)).toEqual({
      reported: true,
      message: 'Ctrl+x runs !echo hi',
    });
  });

  test('the answer is the binding the context selects', () => {
    expect(describeKeyStep([], 'e', table, inList, labelOf)).toEqual({
      reported: true,
      message: 'e runs panel:one',
    });
    expect(describeKeyStep([], 'e', table, { panelType: 'log', textInput: false }, labelOf)).toEqual(
      { reported: true, message: 'e runs global:one' },
    );
  });
});

describe('describeKeyStep — a key that runs nothing', () => {
  test('an unbound key is said to run nothing', () => {
    expect(describeKeyStep([], 'x', table, inList, labelOf)).toEqual({
      reported: true,
      message: 'x is not bound to any command',
    });
  });

  test('a key bound only in other contexts names them and their scope', () => {
    expect(describeKeyStep([], 'f', table, inList, labelOf)).toEqual({
      reported: true,
      message:
        'f is not bound to any command here ' +
        '(elsewhere: file-manager:find in file-manager, treeref:find in treeref, ' +
        'trash:find in trash, recent:find in recent)',
    });
  });

  test('a key a text input swallows is said to be bound when not typing', () => {
    expect(describeKeyStep([], 'z', table, { panelType: null, textInput: true }, labelOf)).toEqual({
      reported: true,
      message:
        'z is not bound to any command here (elsewhere: panel:toggle fullscreen when not typing)',
    });
  });

  test('a widget-scoped key is said to be bound in its widget', () => {
    expect(
      describeKeyStep([], 'ctrl+enter', table, { panelType: 'metarecord-list', textInput: true, focus: 'columns' }, labelOf),
    ).toEqual({
      reported: true,
      message:
        'Ctrl+Enter is not bound to any command here ' +
        '(elsewhere: metarecord-list:apply finder in the finder widget)',
    });
  });

  test('a widget of another panel names the panel too', () => {
    expect(
      describeKeyStep([], 'ctrl+g', table, { panelType: 'log', textInput: false, focus: 'finder' }, labelOf),
    ).toEqual({
      reported: true,
      message:
        'Ctrl+g is not bound to any command here ' +
        '(elsewhere: metarecord-list:toggle normal in the columns widget of metarecord-list)',
    });
  });
});

describe('describeKeyStep — sequences', () => {
  test('a pending sequence waits, listing its continuations', () => {
    expect(describeKeyStep([], 't', table, inList, labelOf)).toEqual({
      pending: true,
      keys: ['t'],
      candidates: [table[4], table[5]],
    });
  });

  test('a sequence that dead-ends runs nothing', () => {
    expect(describeKeyStep(['t'], 'x', table, inList, labelOf)).toEqual({
      reported: true,
      message: 't x is not bound to any command',
    });
  });

  test('the key as it would fire: an exact binding wins over a longer one', () => {
    const prefixes = [b(['g'], 'recent'), b(['g', 'r'], 'script:run')];
    expect(describeKeyStep([], 'g', prefixes, inList, labelOf)).toEqual({
      reported: true,
      message: 'g runs recent',
    });
  });

  test('escape cancels the wait, sequence or not', () => {
    expect(describeKeyStep([], 'escape', table, inList, labelOf)).toEqual({ cancelled: true });
    expect(describeKeyStep(['t'], 'escape', table, inList, labelOf)).toEqual({ cancelled: true });
  });
});
