// The orphan cleanup (doc "Orphans in the GUI"), as the shipped `commands.js`
// defines it since `orphan:delete` / `orphan:detect-delete` left the shell
// builtins: count the marked set, ask about it by name, delete what is marked —
// detection is *not* re-run, so a set the user narrowed by hand is respected
// exactly. `orphan:detect-delete` is the pair in a row.
//
// The action (`mf.orphans`) reports its own outcome; what is pinned here is
// the question and the flow around it — the part a user reshapes.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import shipped from '../../default-config/commands.js';

const calls = {
  orphans: [] as string[],
  invoked: [] as string[],
  status: [] as { kind: string; text: string }[],
};

const state = {
  marked: 3,
  countError: null as Error | null,
  deleteError: null as Error | null,
  detectError: null as Error | null,
};

/** The `mf` a user command is handed (spec-gui "User commands"), faked: the
 *  calls are recorded, the answers and refusals read from `state`. */
function fakeMf() {
  return {
    orphans: {
      count: async () => {
        calls.orphans.push('count');
        if (state.countError) throw state.countError;
        return state.marked;
      },
      delete: async () => {
        calls.orphans.push('delete');
        if (state.deleteError) throw state.deleteError;
        return state.marked;
      },
      detect: async () => {
        calls.orphans.push('detect');
        if (state.detectError) throw state.detectError;
        return state.marked;
      },
    },
    invoke: (invocation: string) => {
      calls.invoked.push(invocation);
      return Promise.resolve({ ok: true });
    },
    statusBar: {
      message: async () => {},
      error: async (error: unknown) => {
        const text = String((error as { message?: unknown } | null)?.message ?? error);
        calls.status.push({ kind: 'error', text });
      },
    },
  };
}

function run(name: 'orphan:delete' | 'orphan:detect-delete') {
  return shipped[name].run(fakeMf() as never);
}

beforeEach(() => {
  for (const list of Object.values(calls)) list.length = 0;
  state.marked = 3;
  state.countError = null;
  state.deleteError = null;
  state.detectError = null;
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe('orphan:delete', () => {
  test('nothing marked: says so, and asks nothing', async () => {
    state.marked = 0;
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);

    await run('orphan:delete');

    expect(confirm).not.toHaveBeenCalled();
    expect(calls.orphans).toEqual(['count']);
    expect(calls.status).toEqual([
      { kind: 'error', text: 'No metarecord is marked orphan = true.' },
    ]);
  });

  test('a count failure is reported — its errors reach no status bar on their own', async () => {
    state.countError = new Error('counting the marked metarecords failed');
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);

    await run('orphan:delete');

    expect(confirm).not.toHaveBeenCalled();
    expect(calls.orphans).toEqual(['count']);
    expect(calls.status).toEqual([
      { kind: 'error', text: 'counting the marked metarecords failed' },
    ]);
  });

  test('the question names the count and what deleting means', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(false);

    await run('orphan:delete');

    const question = String(vi.mocked(window.confirm).mock.calls[0][0]);
    expect(question).toContain('Delete 3 metarecords marked orphan = true?');
    expect(question).toContain(
      'Their files are already gone; the metadata goes with them (undo takes it back).',
    );
  });

  test('one marked metarecord is named in the singular', async () => {
    state.marked = 1;
    vi.spyOn(window, 'confirm').mockReturnValue(false);

    await run('orphan:delete');

    expect(String(vi.mocked(window.confirm).mock.calls[0][0])).toContain(
      'Delete 1 metarecord marked orphan = true?',
    );
  });

  test('Cancel deletes nothing', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(false);

    await run('orphan:delete');

    expect(calls.orphans).toEqual(['count']);
  });

  test('OK deletes the marked set — what is marked is what goes', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(true);

    await run('orphan:delete');

    expect(calls.orphans).toEqual(['count', 'delete']);
  });

  test('a failed deletion is not surfaced twice (the action reports itself)', async () => {
    state.deleteError = new Error('deleting the marked metarecords failed');
    vi.spyOn(window, 'confirm').mockReturnValue(true);

    await run('orphan:delete');

    expect(calls.orphans).toEqual(['count', 'delete']);
    expect(calls.status).toEqual([]);
  });
});

describe('orphan:detect-delete', () => {
  test('detection runs first, then the deletion command', async () => {
    await run('orphan:detect-delete');

    // `mf.invoke('orphan:delete')` is the pair's one confirmation between the
    // two: the deletion command asks its own question.
    expect(calls.orphans).toEqual(['detect']);
    expect(calls.invoked).toEqual(['orphan:delete']);
  });

  test('a detection that fails stops there (it reported its own error)', async () => {
    state.detectError = new Error('this daemon does not support orphan marking');

    await run('orphan:detect-delete');

    // Asking about a marked set detection just failed to refresh would delete
    // on stale information — and re-reporting would duplicate its error.
    expect(calls.orphans).toEqual(['detect']);
    expect(calls.invoked).toEqual([]);
    expect(calls.status).toEqual([]);
  });
});
