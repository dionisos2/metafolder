// `mf:watch-activity reset` (doc "Watch activity"): the GUI half of
// `mf watch activity --reset`. It zeroes the active repository's event counts
// in the daemon, then tells the panels — the counts are in no log, so the
// change feed's poll would never report it and a file manager would keep
// showing the old ones.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { argSpecFor, dispatch } from '../src/lib/commands';
import type { CompletionFn } from '../src/lib/completions';
import { changeFeed } from '../src/lib/panels/api';
import { store } from '../src/lib/store.svelte';

const { invoked, daemon } = vi.hoisted(() => ({
  invoked: [] as { cmd: string; args: any }[],
  daemon: { status: 200, body: {} },
}));

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async (cmd: string, args: unknown) => {
    invoked.push({ cmd, args });
    return cmd === 'daemon_request' ? { status: daemon.status, body: daemon.body } : null;
  }),
  listen: vi.fn(async () => () => {}),
}));

const requests = () => invoked.filter((i) => i.cmd === 'daemon_request').map((i) => i.args);
const statuses = () => invoked.filter((i) => i.cmd === 'post_status').map((i) => i.args);

function focusWorkspace(repo: string | null) {
  store.workspaces = [{ id: 'ws-1', name: 'one', active_repo: repo, repo_name: null }];
  store.layout.left = { ...store.layout.left, workspace_id: 'ws-1' };
  store.layout.focused = 'left';
}

beforeEach(() => {
  invoked.length = 0;
  daemon.status = 200;
  daemon.body = {};
  focusWorkspace('r1');
});

describe('mf:watch-activity', () => {
  test('declares its operation, completing over reset', async () => {
    const spec = argSpecFor('mf:watch-activity');
    expect(spec).toHaveLength(1);
    expect(await (spec![0].complete as CompletionFn)('', [])).toEqual(['reset']);
  });

  test('reset posts the reset for the active repository', async () => {
    expect(await dispatch('mf:watch-activity reset')).toEqual({ ok: true });

    expect(requests()).toEqual([
      { method: 'POST', path: '/repos/r1/watch/activity/reset', body: {} },
    ]);
    expect(statuses()).toContainEqual(
      expect.objectContaining({ text: 'watch activity reset', kind: 'info' }),
    );
  });

  test('tells the panels of that repository to re-read', async () => {
    const heard: unknown[] = [];
    const unsubscribe = changeFeed.subscribe((event) => heard.push(event));

    await dispatch('mf:watch-activity reset');
    unsubscribe();

    expect(heard).toEqual([{ repo: 'r1', uuids: null }]);
  });

  test('a refused reset is reported and nobody is told', async () => {
    daemon.status = 503;
    daemon.body = { error: 'repository is loading' };
    const heard: unknown[] = [];
    const unsubscribe = changeFeed.subscribe((event) => heard.push(event));

    await dispatch('mf:watch-activity reset');
    unsubscribe();

    expect(heard).toEqual([]);
    expect(statuses()).toContainEqual(
      expect.objectContaining({ text: 'repository is loading', kind: 'error' }),
    );
  });

  test('without an active repository nothing is sent', async () => {
    focusWorkspace(null);

    await dispatch('mf:watch-activity reset');

    expect(requests()).toEqual([]);
    expect(statuses()).toContainEqual(expect.objectContaining({ text: 'no active repository' }));
  });

  test('an unknown operation is refused, not run', async () => {
    await dispatch('mf:watch-activity pause');

    expect(requests()).toEqual([]);
    expect(statuses()).toContainEqual(
      expect.objectContaining({
        text: 'unknown operation: "pause" (expected reset)',
      }),
    );
  });
});
