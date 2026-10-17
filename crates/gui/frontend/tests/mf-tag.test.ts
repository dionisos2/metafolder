// `mf:tag` (doc "Tagging from the GUI"): the GUI half of `mf tag`. The rewrite
// is shared Rust behind `tag_answer`; the shell names the selected metarecord,
// completes the answer and the tag paths, and reports.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { argSpecFor, dispatch } from '../src/lib/commands';
import type { CompletionFn } from '../src/lib/completions';
import { store } from '../src/lib/store.svelte';

const { invoked, state } = vi.hoisted(() => {
  const state: {
    selected: { uuid: string; repo: string } | null;
    daemonBody: Record<string, string[]>;
    fail: string | null;
  } = { selected: { uuid: 'u1', repo: 'r1' }, daemonBody: {}, fail: null };
  return { invoked: [] as { cmd: string; args: any }[], state };
});

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async (cmd: string, args: any) => {
    invoked.push({ cmd, args });
    if (cmd === 'ws_get_var') return args.key === 'selected_metarecord' ? state.selected : null;
    if (cmd === 'daemon_request') return { status: 200, body: state.daemonBody };
    if (cmd === 'tag_answer') {
      if (state.fail) throw new Error(state.fail);
      return { written: 1, message: `Tag: ${args.path} added` };
    }
    return null;
  }),
  listen: vi.fn(async () => () => {}),
}));

const calls = (cmd: string) => invoked.filter((i) => i.cmd === cmd).map((i) => i.args);

beforeEach(() => {
  invoked.length = 0;
  state.selected = { uuid: 'u1', repo: 'r1' };
  state.daemonBody = {};
  state.fail = null;
  store.workspaces = [{ id: 'ws-1', name: 'one', active_repo: 'r1', repo_name: null }];
  store.layout.left = { ...store.layout.left, workspace_id: 'ws-1' };
  store.layout.focused = 'left';
});

describe('mf:tag', () => {
  test('completes the answer, then every tag path', async () => {
    const spec = argSpecFor('mf:tag')!;
    expect(spec).toHaveLength(2);
    expect(await (spec[0].complete as CompletionFn)('', [])).toEqual([
      'add',
      'deny',
      'mixed',
      'remove',
    ]);
    state.daemonBody = { a: ['musique/jazz'], b: ['sport/maraton', 'évènement/maraton'] };
    expect(await (spec[1].complete as CompletionFn)('', ['add'])).toEqual([
      'musique/jazz',
      'sport/maraton',
      'évènement/maraton',
    ]);
    expect(calls('daemon_request')).toEqual([
      {
        method: 'POST',
        path: '/repos/r1/query/fields/resolve-tree',
        body: {
          query: { type: 'eq', field: 'mf_schema', value: { type: 'string', value: 'tag' } },
          field: 'path',
        },
      },
    ]);
  });

  test('answers about the tag on the selected metarecord and reports it', async () => {
    expect(await dispatch('mf:tag add jeux video')).toEqual({ ok: true });
    // The tag is the last argument: its words arrive joined.
    expect(calls('tag_answer')).toEqual([
      { repo: 'r1', uuid: 'u1', verb: 'add', path: 'jeux video' },
    ]);
    expect(calls('post_status')).toContainEqual(
      expect.objectContaining({ text: 'Tag: jeux video added', kind: 'info' }),
    );
  });

  test('says so when nothing is selected, and writes nothing', async () => {
    state.selected = null;
    await dispatch('mf:tag deny musique');
    expect(calls('tag_answer')).toEqual([]);
    expect(calls('post_status')).toContainEqual(
      expect.objectContaining({ text: 'no metarecord is selected' }),
    );
  });

  test('a refused answer is reported', async () => {
    state.fail = 'parent path does not exist';
    await dispatch('mf:tag add musique');
    expect(calls('post_status')).toContainEqual(
      expect.objectContaining({ text: 'parent path does not exist', kind: 'error' }),
    );
  });
});
