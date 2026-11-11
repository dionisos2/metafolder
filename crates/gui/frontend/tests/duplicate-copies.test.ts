// `mf:duplicate keep|trash query|selection` (doc "Deleting duplicate copies"):
// the GUI half of `mf duplicate keep` / `trash`. The copies to trash are a
// query built on `same`, over the list's query or the checked metarecords; it
// lists them, asks, then trashes them one by one.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { argSpecFor, dispatch } from '../src/lib/commands';
import type { CompletionFn } from '../src/lib/completions';
import { store } from '../src/lib/store.svelte';

const { invoked, state } = vi.hoisted(() => {
  const state: {
    vars: Record<string, unknown>;
    results: unknown[];
    paths: Record<string, unknown>;
    trashFails: string | null;
  } = { vars: {}, results: [], paths: {}, trashFails: null };
  return { invoked: [] as { cmd: string; args: any }[], state };
});

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async (cmd: string, args: any) => {
    invoked.push({ cmd, args });
    if (cmd === 'ws_get_var') return state.vars[args.key] ?? null;
    if (cmd === 'trash_path') {
      if (state.trashFails && args.path === state.trashFails) throw new Error('refused');
      return { desktop: false };
    }
    if (cmd !== 'daemon_request') return null;
    if (args.method === 'GET' && args.path === '/repos')
      return { status: 200, body: [{ repo_uuid: 'r1', root: '/root' }] };
    if (args.path === '/repos/r1/query')
      return { status: 200, body: { results: state.results, next_cursor: null } };
    if (args.path === '/repos/r1/query/fields/resolve-tree')
      return { status: 200, body: state.paths };
    return { status: 200, body: {} };
  }),
  listen: vi.fn(async () => () => {}),
}));

const LIST_QUERY = { type: 'is_present', field: 'kept' };
const same = (target: unknown) => ({
  type: 'same_as',
  field: 'mfr_duplicate_group',
  target,
});
const not = (operand: unknown) => ({ type: 'not', operand });

const requests = () => invoked.filter((i) => i.cmd === 'daemon_request').map((i) => i.args);
const queried = () => requests().filter((r) => r.path === '/repos/r1/query');
const trashed = () => invoked.filter((i) => i.cmd === 'trash_path').map((i) => i.args.path);
const statuses = () =>
  invoked.filter((i) => i.cmd === 'post_status').map((i) => i.args.text as string);

const copy = (uuid: string, size: number, inode = false) => ({
  uuid,
  fields: [
    { name: 'mfr_duplicate_group', value: { type: 'ref', value: `g-${uuid}` } },
    { name: 'mfr_size', value: { type: 'int', value: size } },
    ...(inode ? [{ name: 'mfr_inode', value: { type: 'string', value: '1:2' } }] : []),
  ],
});

let confirm: ReturnType<typeof vi.fn>;

beforeEach(() => {
  invoked.length = 0;
  state.vars = { 'metarecord-list:effective-query': LIST_QUERY };
  state.results = [copy('u1', 1024), copy('u2', 2048)];
  state.paths = { u1: ['/b/x.txt'], u2: ['/a/y.txt'] };
  state.trashFails = null;
  store.workspaces = [{ id: 'ws-1', name: 'one', active_repo: 'r1', repo_name: null }];
  store.layout.left = { ...store.layout.left, workspace_id: 'ws-1' };
  store.layout.focused = 'left';
  confirm = vi.fn(() => true);
  vi.stubGlobal('confirm', confirm);
});

afterEach(() => vi.unstubAllGlobals());

describe('mf:duplicate keep / trash', () => {
  test('the operation completes over scan, keep and trash', async () => {
    const spec = argSpecFor('mf:duplicate')!;
    expect(await (spec[0].complete as CompletionFn)('', [])).toEqual(['scan', 'keep', 'trash']);
  });

  test('keep and trash ask what they act on; scan does not', async () => {
    const source = argSpecFor('mf:duplicate')![1];
    expect(source.when!(['keep'])).toBe(true);
    expect(source.when!(['trash'])).toBe(true);
    expect(source.when!(['scan'])).toBe(false);
    expect(await (source.complete as CompletionFn)('', ['keep'])).toEqual(['query', 'selection']);
  });

  test('keep query trashes the copies outside the list query, in its groups', async () => {
    expect(await dispatch('mf:duplicate keep query')).toEqual({ ok: true });

    expect(queried()[0].body.query).toEqual({
      type: 'and',
      operands: [same(LIST_QUERY), not(LIST_QUERY)],
    });
    // Sorted by path, joined to the repository root.
    expect(trashed()).toEqual(['/root/a/y.txt', '/root/b/x.txt']);
    const question = confirm.mock.calls[0][0] as string;
    expect(question).toContain('/a/y.txt');
    expect(question).toContain('2 groups');
    expect(question).toContain('3.0K');
    expect(invoked).toContainEqual(
      expect.objectContaining({
        cmd: 'ws_set_var',
        args: expect.objectContaining({ key: 'metarecords:dirty' }),
      }),
    );
    expect(statuses()).toContain('Trashed 2 duplicate copies');
  });

  test('trash selection trashes the checked copies with a twin elsewhere', async () => {
    state.vars.selected_metarecords = ['c1', 'c2'];
    const checked = { type: 'uuid_in', uuids: ['c1', 'c2'] };

    await dispatch('mf:duplicate trash selection');

    expect(queried()[0].body.query).toEqual({
      type: 'and',
      operands: [checked, same(not(checked))],
    });
    expect(trashed()).toHaveLength(2);
  });

  test('a hard link is not counted in the space reclaimed', async () => {
    state.results = [copy('u1', 1024), copy('u2', 2048, true)];

    await dispatch('mf:duplicate keep query');

    expect(confirm.mock.calls[0][0]).toContain('1.0K');
  });

  test('declining the question trashes nothing', async () => {
    confirm.mockReturnValue(false);

    await dispatch('mf:duplicate keep query');

    expect(trashed()).toEqual([]);
  });

  test('nothing to trash is said, and nothing is asked', async () => {
    state.results = [];

    await dispatch('mf:duplicate keep query');

    expect(confirm).not.toHaveBeenCalled();
    expect(statuses()).toContain('No duplicate copy to trash');
  });

  test('a list without a query is refused before any request', async () => {
    state.vars = {};

    await dispatch('mf:duplicate keep query');

    expect(queried()).toEqual([]);
    expect(statuses()).toContain('The metarecord list shows no query — no copy to keep.');
  });

  test('an empty selection is refused, never the query instead', async () => {
    await dispatch('mf:duplicate trash selection');

    expect(queried()).toEqual([]);
    expect(statuses()).toContain('Nothing is checked — no copy to trash.');
  });

  test('a refused trashing stops the run and says how far it got', async () => {
    state.trashFails = '/root/a/y.txt';

    await dispatch('mf:duplicate keep query');

    expect(trashed()).toEqual(['/root/a/y.txt']);
    expect(statuses()).toContainEqual(expect.stringContaining('refused (after trashing 0 of 2)'));
  });

  test('an unknown source is refused', async () => {
    await dispatch('mf:duplicate keep everything');

    expect(queried()).toEqual([]);
    expect(statuses()).toContain('unknown source: "everything" (expected query or selection)');
  });
});
