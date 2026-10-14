// The shared "jump to an entry by name" helper (doc "Jumping to an entry"):
// every list panel with a row cursor registers its `<panel>:find` command with
// it, so they all complete, match and fail the same way — and can all sit on
// the same key.

import { describe, expect, test, vi } from 'vitest';
import { matchEntry, registerFind } from '../../panel-shim/find-entry.js';

const rows = [
  { name: 'sub', label: 'sub/' },
  { name: 'notes.txt' },
  { name: 'top.txt' },
];

describe('matchEntry', () => {
  test('an exact candidate label wins (what accepting a completion gives)', () => {
    expect(matchEntry('sub/', rows)).toBe(0);
  });

  test('an exact name matches even when the label decorates it', () => {
    expect(matchEntry('sub', rows)).toBe(0);
  });

  test('surrounding whitespace is ignored', () => {
    expect(matchEntry('  top.txt  ', rows)).toBe(2);
  });

  test('a typed value matches by ordered substring', () => {
    expect(matchEntry('no txt', rows)).toBe(1);
  });

  test('ordered substring picks the first matching row', () => {
    expect(matchEntry('txt', rows)).toBe(1);
  });

  test('the ordered substring runs over the label, so its context is searchable', () => {
    // A panel whose rows are ambiguous by name alone (the trash, the recent
    // list) labels each with the path that tells them apart — and typing that
    // context must therefore find the row.
    const paths = [
      { name: 'x.txt', label: 'x.txt — /r/one/x.txt' },
      { name: 'x.txt', label: 'x.txt — /r/two/x.txt' },
    ];
    expect(matchEntry('two x', paths)).toBe(1);
  });

  test('no match is -1', () => {
    expect(matchEntry('zzz', rows)).toBe(-1);
  });

  test('an empty answer matches nothing (rather than everything)', () => {
    expect(matchEntry('   ', rows)).toBe(-1);
  });
});

/** A stub metafolder API capturing what a panel registers. */
function stubApi() {
  const handlers = new Map<string, (...a: string[]) => unknown>();
  const specs = new Map<string, { name: string; prompt: () => unknown; complete?: () => unknown }[]>();
  const statusBar = { error: vi.fn(async () => {}), message: vi.fn(async () => {}) };
  return {
    handlers,
    specs,
    statusBar,
    api: {
      settings: { statusErrorMs: 2000 },
      statusBar,
      commands: {
        register: vi.fn(
          (
            name: string,
            opts: {
              handler?: (...a: string[]) => unknown;
              args?: { name: string; prompt: () => unknown; complete?: () => unknown }[];
            },
          ) => {
            if (opts.handler) handlers.set(name, opts.handler);
            if (opts.args) specs.set(name, opts.args);
            return Promise.resolve(null);
          },
        ),
      },
    },
  };
}

describe('registerFind', () => {
  function setup(entries = rows) {
    const s = stubApi();
    const select = vi.fn(async (_index: number) => {});
    void registerFind(s.api as never, 'demo:find', {
      label: 'Demo: jump to an entry by name',
      prompt: 'Go to entry:',
      entries: () => entries,
      select,
    });
    return { ...s, select };
  }

  test('completes over the rows, showing each row label', async () => {
    const s = setup();
    const args = s.specs.get('demo:find')!;
    expect(args).toHaveLength(1);
    expect(await args[0].prompt()).toBe('Go to entry:');
    expect(await args[0].complete!()).toEqual(['sub/', 'notes.txt', 'top.txt']);
  });

  test('an answer moves the cursor onto the row it designates', async () => {
    const s = setup();
    await s.handlers.get('demo:find')!('no txt');
    expect(s.select).toHaveBeenCalledWith(1);
    expect(s.statusBar.error).not.toHaveBeenCalled();
  });

  test('no match is a status-bar error and leaves the cursor alone', async () => {
    const s = setup();
    await s.handlers.get('demo:find')!('zzz');
    expect(s.select).not.toHaveBeenCalled();
    expect(s.statusBar.error).toHaveBeenCalledWith('no entry matching "zzz"', 2000);
  });

  test('an empty answer is a silent no-op (the question was abandoned)', async () => {
    const s = setup();
    await s.handlers.get('demo:find')!('  ');
    expect(s.select).not.toHaveBeenCalled();
    expect(s.statusBar.error).not.toHaveBeenCalled();
  });

  test('the rows are re-read at each call, never captured at registration', async () => {
    let entries = [{ name: 'one' }];
    const s = stubApi();
    const select = vi.fn(async (_i: number) => {});
    void registerFind(s.api as never, 'demo:find', {
      label: 'Demo',
      entries: () => entries,
      select,
    });
    entries = [{ name: 'one' }, { name: 'two' }];
    expect(await s.specs.get('demo:find')![0].complete!()).toEqual(['one', 'two']);
    await s.handlers.get('demo:find')!('two');
    expect(select).toHaveBeenCalledWith(1);
  });
});

// The walking variant (doc "Jumping to an entry"): a panel that can *enter* a
// row (a directory) registers its find with an `enter` callback, and the
// command then takes a first `select | walk` argument. `walk` enters what can
// be entered and asks again from inside; a Shift-Enter answer selects instead.
describe('registerFind with enter', () => {
  function setup() {
    const s = stubApi();
    const invoke = vi.fn(() => null);
    (s.api.commands as Record<string, unknown>).invoke = invoke;
    const select = vi.fn(async (_index: number) => {});
    const enter = vi.fn(async (index: number) => rows[index].name === 'sub');
    void registerFind(s.api as never, 'demo:find', {
      label: 'Demo',
      entries: () => rows,
      select,
      enter,
    });
    return { ...s, select, enter, invoke };
  }

  test('the mode is asked first, the entry second', async () => {
    const s = setup();
    const args = s.specs.get('demo:find')! as unknown as {
      name: string;
      complete?: () => unknown;
      alternate?: { hint: string; prior: (p: string[]) => string[] };
    }[];
    expect(args.map((a) => a.name)).toEqual(['mode', 'entry']);
    expect(await args[0].complete!()).toEqual(['select', 'walk']);
    expect(args[1].alternate!.prior(['walk'])).toEqual(['select']);
  });

  test('select mode selects a directory without entering it', async () => {
    const s = setup();
    await s.handlers.get('demo:find')!('select', 'sub/');
    expect(s.select).toHaveBeenCalledWith(0);
    expect(s.enter).not.toHaveBeenCalled();
  });

  test('walk mode enters a directory and asks again', async () => {
    const s = setup();
    await s.handlers.get('demo:find')!('walk', 'sub/');
    expect(s.enter).toHaveBeenCalledWith(0);
    expect(s.select).not.toHaveBeenCalled();
    expect(s.invoke).toHaveBeenCalledWith('demo:find walk');
  });

  test('walk mode selects what cannot be entered, and stops', async () => {
    const s = setup();
    await s.handlers.get('demo:find')!('walk', 'top.txt');
    expect(s.select).toHaveBeenCalledWith(2);
    expect(s.invoke).not.toHaveBeenCalled();
  });

  test('walk mode with no match reports it and stops', async () => {
    const s = setup();
    await s.handlers.get('demo:find')!('walk', 'zzz');
    expect(s.statusBar.error).toHaveBeenCalled();
    expect(s.invoke).not.toHaveBeenCalled();
  });

  test('an unknown mode is an error', async () => {
    const s = setup();
    await s.handlers.get('demo:find')!('dive', 'sub/');
    expect(s.statusBar.error).toHaveBeenCalledWith('unknown find mode "dive" (select / walk)', 2000);
    expect(s.select).not.toHaveBeenCalled();
  });
});
