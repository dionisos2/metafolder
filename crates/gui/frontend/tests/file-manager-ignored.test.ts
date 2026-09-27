// file-manager exclusion marking (spec-gui "Ignore patterns"): a listing's
// excluded entries — by an mf_ignore pattern or by mf_watch = false — come from
// a single POST /eligibility, and the same call yields the tracking scope the
// ad-hoc patterns must be anchored at.

import { describe, expect, test, vi } from 'vitest';
import {
  loadEligibility,
  scopedPath,
} from '../../default-config/panel-types/file-manager/ignored.js';

function daemonStub(results: unknown[]) {
  const call = vi.fn(async (_method: string, _path: string, _body?: unknown) => ({ results }));
  return { daemon: { call } as any, call };
}

describe('scopedPath', () => {
  test('re-anchors a repo-relative path at the tracking scope', () => {
    expect(scopedPath('/work/live/a.txt', '/work')).toBe('/live/a.txt');
    expect(scopedPath('/work/live', '/work/live')).toBe('');
    expect(scopedPath('/a.txt', '')).toBe('/a.txt');
    expect(scopedPath('/a.txt', null)).toBe('/a.txt');
  });
});

describe('loadEligibility', () => {
  test('asks about the directory and its entries in one call', async () => {
    const { daemon, call } = daemonStub([
      { path: '/work', eligible: true, reason: 'tracked', watch_scope: '', ignore_source: '' },
      {
        path: '/work/target',
        eligible: false,
        reason: 'ignored',
        watch_scope: '',
        ignore_source: '/work',
        pattern: 'target(/.*)?$',
      },
      { path: '/work/src', eligible: true, reason: 'tracked', watch_scope: '', ignore_source: '' },
    ]);
    const result = await loadEligibility(daemon, 'repo1', '/data/repo', '/data/repo/work', [
      '/data/repo/work/target',
      '/data/repo/work/src',
    ]);

    expect(call).toHaveBeenCalledTimes(1);
    const [method, path, body] = call.mock.calls[0];
    expect(method).toBe('POST');
    expect(path).toBe('/repos/repo1/eligibility');
    expect(body).toEqual({ paths: ['/work', '/work/target', '/work/src'] });

    expect(result.scope).toBe('');
    expect([...result.excluded.keys()]).toEqual(['/data/repo/work/target']);
    expect(result.excluded.get('/data/repo/work/target')).toEqual({
      reason: 'ignored',
      pattern: 'target(/.*)?$',
      source: '/work',
    });
  });

  test('an unwatched repo is not marked: the whole listing shares the directory scope', async () => {
    const { daemon } = daemonStub([
      { path: '', eligible: false, reason: 'watch_false', watch_scope: '', ignore_source: null },
      { path: '/a', eligible: false, reason: 'watch_false', watch_scope: '', ignore_source: null },
    ]);
    const result = await loadEligibility(daemon, 'repo1', '/data/repo', '/data/repo', [
      '/data/repo/a',
    ]);
    expect(result.excluded.size).toBe(0);
  });

  test('mf_watch = false is marked where it starts: an entry whose scope is not the directory\'s', async () => {
    const { daemon } = daemonStub([
      { path: '/cfg', eligible: true, reason: 'tracked', watch_scope: '', ignore_source: null },
      {
        path: '/cfg/db',
        eligible: false,
        reason: 'watch_false',
        watch_scope: '/cfg/db',
        ignore_source: null,
      },
      { path: '/cfg/x', eligible: true, reason: 'tracked', watch_scope: '', ignore_source: null },
    ]);
    const result = await loadEligibility(daemon, 'repo1', '/data/repo', '/data/repo/cfg', [
      '/data/repo/cfg/db',
      '/data/repo/cfg/x',
    ]);
    expect([...result.excluded.keys()]).toEqual(['/data/repo/cfg/db']);
    expect(result.excluded.get('/data/repo/cfg/db')).toEqual({
      reason: 'watch_false',
      source: '/cfg/db',
    });
  });

  test('inside an unwatched subtree the entries are not marked one by one', async () => {
    const { daemon } = daemonStub([
      { path: '/cfg/db', eligible: false, reason: 'watch_false', watch_scope: '/cfg/db' },
      { path: '/cfg/db/f', eligible: false, reason: 'watch_false', watch_scope: '/cfg/db' },
    ]);
    const result = await loadEligibility(daemon, 'repo1', '/data/repo', '/data/repo/cfg/db', [
      '/data/repo/cfg/db/f',
    ]);
    expect(result.excluded.size).toBe(0);
  });

  test('without the directory\'s own answer, no mf_watch = false boundary is guessed', async () => {
    const { daemon } = daemonStub([
      { path: '/a', eligible: false, reason: 'watch_false', watch_scope: '' },
    ]);
    const result = await loadEligibility(daemon, 'repo1', '/data/repo', '/data/repo/d', [
      '/data/repo/a',
    ]);
    expect(result.excluded.size).toBe(0);
  });

  test('paths outside the repository are dropped, and a listing wholly outside skips the call', async () => {
    const { daemon, call } = daemonStub([]);
    const result = await loadEligibility(daemon, 'repo1', '/data/repo', '/etc', ['/etc/passwd']);
    expect(call).not.toHaveBeenCalled();
    expect(result.excluded.size).toBe(0);
    expect(result.scope).toBe(null);
  });

  test('no repository means no call', async () => {
    const { daemon, call } = daemonStub([]);
    const result = await loadEligibility(daemon, null, null, '/data/repo', ['/data/repo/a']);
    expect(call).not.toHaveBeenCalled();
    expect(result.excluded.size).toBe(0);
  });

  test('a daemon failure degrades to "nothing is known to be ignored"', async () => {
    const daemon = {
      call: vi.fn(async () => {
        throw new Error('daemon down');
      }),
    } as any;
    const result = await loadEligibility(daemon, 'repo1', '/data/repo', '/data/repo', [
      '/data/repo/a',
    ]);
    expect(result.excluded.size).toBe(0);
    expect(result.scope).toBe(null);
  });
});
