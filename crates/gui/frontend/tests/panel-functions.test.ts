// Generic panel commands (doc "Generic panel commands"): a panel provides a few
// functions (`metafolder.provide`), and one shell command — `cursor:next
// <panel>` — works in every panel that provides what it needs. The panel is a
// required argument: a panel type, or `self` for the focused one.

import { describe, expect, test, vi } from 'vitest';
import {
  appliesHere,
  matchEntry,
  missingFunctions,
  neededFunctions,
  resolvePanel,
  runGeneric,
  type PanelFunctions,
} from '../src/lib/panelFunctions';

/** A list of `n` rows with its cursor at `at`, as a panel would provide it. */
function list(n: number, at = -1) {
  const state = { at, n };
  const fns = {
    index: () => state.at,
    count: () => state.n,
    select: vi.fn((i: number) => {
      state.at = i;
    }),
  } satisfies PanelFunctions;
  return { state, fns };
}

const run = (name: string, fns: PanelFunctions, rest: string[] = [], reinvoke = vi.fn()) =>
  runGeneric(name, fns, rest, { panelType: 'trash', reinvoke });

describe('resolvePanel', () => {
  test('self is the focused panel type', () => {
    expect(resolvePanel('self', 'trash')).toBe('trash');
  });

  test('a panel type names itself, whatever has the focus', () => {
    expect(resolvePanel('metarecord-list', 'file')).toBe('metarecord-list');
  });

  test('self without a focused panel is an error', () => {
    expect(() => resolvePanel('self', null)).toThrow(/no focused panel/);
  });

  test('the panel is required', () => {
    expect(() => resolvePanel(undefined, 'trash')).toThrow(/which panel/);
    expect(() => resolvePanel('', 'trash')).toThrow(/which panel/);
  });
});

describe('neededFunctions', () => {
  test('moving one row needs the index, the count and select', () => {
    expect(neededFunctions('cursor:next', [])).toEqual(['index', 'count', 'select']);
    expect(neededFunctions('cursor:prev', [])).toEqual(['index', 'count', 'select']);
  });

  test('the two ends need no index', () => {
    expect(neededFunctions('cursor:first', [])).toEqual(['count', 'select']);
    expect(neededFunctions('cursor:last', [])).toEqual(['count', 'select']);
  });

  test('find needs entries, and enter only to walk', () => {
    expect(neededFunctions('cursor:find', ['select'])).toEqual(['entries', 'select']);
    expect(neededFunctions('cursor:find', ['walk'])).toEqual(['entries', 'select', 'enter']);
  });

  test('a command that is not generic needs nothing — null', () => {
    expect(neededFunctions('panel:swap', [])).toBeNull();
  });
});

describe('missingFunctions', () => {
  test('lists what is needed and not provided, in order', () => {
    expect(missingFunctions(new Set(['select']), ['index', 'count', 'select'])).toEqual([
      'index',
      'count',
    ]);
  });

  test('a panel that provides nothing lacks everything', () => {
    expect(missingFunctions(undefined, ['count', 'select'])).toEqual(['count', 'select']);
  });
});

describe('appliesHere — whether a key bound to a generic command fires', () => {
  const provided = (type: string) =>
    ({
      trash: new Set(['index', 'count', 'select', 'entries']),
      'metarecord-list': new Set(['index', 'count', 'select']),
    })[type];

  test('self applies in a panel providing what the command needs', () => {
    expect(appliesHere('cursor:next self', 'trash', provided)).toBe(true);
  });

  test('self does not apply in a panel that provides nothing (the key passes through)', () => {
    expect(appliesHere('cursor:next self', 'help', provided)).toBe(false);
  });

  test('nor in one that lacks a needed function', () => {
    expect(appliesHere('cursor:find self walk', 'trash', provided)).toBe(false);
    expect(appliesHere('cursor:find self select', 'trash', provided)).toBe(true);
  });

  test('a named panel is asked, not the focused one', () => {
    expect(appliesHere('cursor:next metarecord-list', 'file', provided)).toBe(true);
  });

  test('self without a focused panel does not apply', () => {
    expect(appliesHere('cursor:next self', null, provided)).toBe(false);
  });

  test('any other command always applies', () => {
    expect(appliesHere('panel:swap', 'help', provided)).toBe(true);
    expect(appliesHere('trash:restore', 'help', provided)).toBe(true);
  });
});

describe('runGeneric — moving the cursor', () => {
  test('next moves one row down', async () => {
    const { fns, state } = list(5, 1);
    await run('cursor:next', fns);
    expect(state.at).toBe(2);
  });

  test('next stops on the last row', async () => {
    const { fns, state } = list(3, 2);
    await run('cursor:next', fns);
    expect(state.at).toBe(2);
  });

  test('next from no selection lands on the first row', async () => {
    const { fns, state } = list(3, -1);
    await run('cursor:next', fns);
    expect(state.at).toBe(0);
  });

  test('prev moves one row up, and stops on the first', async () => {
    const { fns, state } = list(3, 1);
    await run('cursor:prev', fns);
    expect(state.at).toBe(0);
    await run('cursor:prev', fns);
    expect(state.at).toBe(0);
  });

  test('prev from no selection lands on the first row', async () => {
    const { fns, state } = list(3, -1);
    await run('cursor:prev', fns);
    expect(state.at).toBe(0);
  });

  test('first and last go to the two ends', async () => {
    const { fns, state } = list(4, 1);
    await run('cursor:last', fns);
    expect(state.at).toBe(3);
    await run('cursor:first', fns);
    expect(state.at).toBe(0);
  });

  test('an empty list is left alone', async () => {
    const { fns } = list(0, -1);
    for (const name of ['cursor:next', 'cursor:prev', 'cursor:first', 'cursor:last']) {
      await run(name, fns);
    }
    expect(fns.select).not.toHaveBeenCalled();
  });

  test('next on the last loaded row asks for more first (a paged list)', async () => {
    const { fns, state } = list(2, 1);
    const more = vi.fn(() => {
      state.n = 4;
    });
    await run('cursor:next', { ...fns, more });
    expect(more).toHaveBeenCalledOnce();
    expect(state.at).toBe(2);
  });

  test('more is not asked for before the last row', async () => {
    const { fns } = list(3, 0);
    const more = vi.fn();
    await run('cursor:next', { ...fns, more });
    expect(more).not.toHaveBeenCalled();
  });

  test('a panel lacking a needed function is an error naming the command and the panel', async () => {
    await expect(run('cursor:next', { select: () => {} })).rejects.toThrow(
      'cursor:next is not implemented for the trash panel',
    );
  });
});

describe('runGeneric — finding an entry', () => {
  const rows = [{ name: 'sub', label: 'sub/' }, { name: 'notes.txt' }, { name: 'top.txt' }];

  test('select moves the cursor onto the entry named', async () => {
    const select = vi.fn();
    await run('cursor:find', { entries: () => rows, select }, ['select', 'top.txt']);
    expect(select).toHaveBeenCalledWith(2);
  });

  test('an answer matching nothing is an error', async () => {
    await expect(
      run('cursor:find', { entries: () => rows, select: () => {} }, ['select', 'zzz']),
    ).rejects.toThrow('no entry matching "zzz"');
  });

  test('an empty answer is a silent no-op (the question was abandoned)', async () => {
    const select = vi.fn();
    await run('cursor:find', { entries: () => rows, select }, ['select', '  ']);
    expect(select).not.toHaveBeenCalled();
  });

  test('an unknown mode is an error', async () => {
    await expect(
      run('cursor:find', { entries: () => rows, select: () => {} }, ['dive', 'sub']),
    ).rejects.toThrow(/unknown find mode "dive"/);
  });

  test('walk enters what can be entered, and asks again for the same panel', async () => {
    const select = vi.fn();
    const enter = vi.fn(async () => true);
    const reinvoke = vi.fn();
    await run('cursor:find', { entries: () => rows, select, enter }, ['walk', 'sub'], reinvoke);
    expect(enter).toHaveBeenCalledWith(0);
    expect(select).not.toHaveBeenCalled();
    expect(reinvoke).toHaveBeenCalledWith('cursor:find trash walk');
  });

  test('walk selects (and stops at) what cannot be entered', async () => {
    const select = vi.fn();
    const reinvoke = vi.fn();
    const enter = vi.fn(async () => false);
    await run('cursor:find', { entries: () => rows, select, enter }, ['walk', 'top'], reinvoke);
    expect(select).toHaveBeenCalledWith(2);
    expect(reinvoke).not.toHaveBeenCalled();
  });

  test("a row's own index is where the cursor goes, when the rows skip some", async () => {
    // The file manager searches its real entries, not the synthetic "." and
    // ".." heading the listing: each row names the listing row it stands for.
    const select = vi.fn();
    const enter = vi.fn(async () => true);
    const skipping = () => [
      { name: 'sub', index: 2 },
      { name: 'top.txt', index: 3 },
    ];
    await run('cursor:find', { entries: skipping, select }, ['select', 'top.txt']);
    expect(select).toHaveBeenCalledWith(3);
    await run('cursor:find', { entries: skipping, select, enter }, ['walk', 'sub']);
    expect(enter).toHaveBeenCalledWith(2);
  });

  test('walk in a panel that cannot enter is not implemented', async () => {
    await expect(
      run('cursor:find', { entries: () => rows, select: () => {} }, ['walk', 'sub']),
    ).rejects.toThrow('cursor:find walk is not implemented for the trash panel');
  });
});

describe('matchEntry', () => {
  const rows = [{ name: 'sub', label: 'sub/' }, { name: 'notes.txt' }, { name: 'top.txt' }];

  test('an exact candidate label wins (what accepting a completion gives)', () => {
    expect(matchEntry('sub/', rows)).toBe(0);
  });

  test('an exact name matches even when the label decorates it', () => {
    expect(matchEntry('sub', rows)).toBe(0);
  });

  test('surrounding whitespace is ignored', () => {
    expect(matchEntry('  top.txt  ', rows)).toBe(2);
  });

  test('a typed value matches by ordered substring, the first row shown', () => {
    expect(matchEntry('no txt', rows)).toBe(1);
    expect(matchEntry('txt', rows)).toBe(1);
  });

  test('the ordered substring runs over the label, so its context is searchable', () => {
    const paths = [
      { name: 'x.txt', label: 'x.txt — /r/one/x.txt' },
      { name: 'x.txt', label: 'x.txt — /r/two/x.txt' },
    ];
    expect(matchEntry('two x', paths)).toBe(1);
  });

  test('no match, or an empty answer, is -1', () => {
    expect(matchEntry('zzz', rows)).toBe(-1);
    expect(matchEntry('   ', rows)).toBe(-1);
  });
});
