// The watch-state shim (/__watched.js, spec-file-tracking "Watch check"):
// phrasing the daemon's per-path answers, and a metarecord's verdict from the
// results of its several paths. The daemon call itself is thin — the phrasing
// is what a user reads.

import { describe, expect, test } from 'vitest';
import { summarizeWatched, watchedLabel } from '../../panel-shim/watched.js';

/** One daemon result, with the fields every phrasing branch reads. */
function result(overrides = {}) {
  return {
    path: '/notes.txt',
    watched: true,
    reason: 'watched',
    watched_dir: '',
    eligible: true,
    eligibility_reason: 'tracked',
    watch_scope: '',
    ignore_source: null,
    pattern: null,
    dir_eligible: true,
    dir_eligibility_reason: 'tracked',
    dir_watch_scope: '',
    dir_ignore_source: null,
    dir_pattern: null,
    excluded_by: null,
    offline_mount: null,
    ...overrides,
  };
}

describe('watchedLabel', () => {
  test('a watched path says what covers it', () => {
    expect(watchedLabel(result())).toBe('watched — changes under / are recorded');
    expect(watchedLabel(result({ watched_dir: '/docs/notes' }))).toBe(
      'watched — changes under /docs/notes are recorded',
    );
  });

  test('an untracked path blames the deciding field', () => {
    expect(
      watchedLabel(
        result({
          watched: false,
          reason: 'untracked',
          eligible: false,
          eligibility_reason: 'no_watch',
        }),
      ),
    ).toBe(
      'not watched — not tracked: no mf_watch on it or its ancestors (tracking is opt-in)',
    );
    expect(
      watchedLabel(
        result({
          watched: false,
          reason: 'untracked',
          eligible: false,
          eligibility_reason: 'watch_false',
          watch_scope: '/archive',
        }),
      ),
    ).toBe('not watched — not tracked: mf_watch = false inherited from /archive');
    expect(
      watchedLabel(
        result({
          watched: false,
          reason: 'untracked',
          eligible: false,
          eligibility_reason: 'ignored',
          ignore_source: '',
          pattern: '(^|/)target/',
        }),
      ),
    ).toBe('not watched — not tracked: matches the mf_ignore pattern "(^|/)target/" of /');
  });

  test('a file beneath a pruned directory blames the directory', () => {
    // The file's own name matches nothing, but its directory is pruned
    // (cascading skip), so it can never be reached.
    const label = watchedLabel(
      result({
        watched: false,
        reason: 'untracked',
        eligible: true,
        watched_dir: '/build',
        dir_eligibility_reason: 'ignored',
        dir_ignore_source: '',
        dir_pattern: '(^|/)build$',
      }),
    );
    expect(label).toBe(
      'not watched — its directory /build is untracked: matches the mf_ignore pattern "(^|/)build$" of /',
    );
  });

  test('an exclusion, an unplugged volume, the internals and starvation name themselves', () => {
    expect(
      watchedLabel(result({ watched: false, reason: 'excluded', excluded_by: '/big' })),
    ).toContain('inside /big (mfr_watch_exceeded');
    expect(
      watchedLabel(result({ watched: false, reason: 'offline', offline_mount: '/media/photos' })),
    ).toBe('not watched — on the volume mounted at /media/photos, which is unplugged');
    expect(watchedLabel(result({ watched: false, reason: 'internal' }))).toContain(
      'runtime directory',
    );
    expect(
      watchedLabel(result({ watched: false, reason: 'unwatched', watched_dir: '/docs' })),
    ).toContain('no watch on /docs right now');
  });
});

describe('summarizeWatched', () => {
  test('watched when any of the paths is', () => {
    const summary = summarizeWatched([result(), result({ path: '/old.txt', watched: false })]);
    expect(summary?.watched).toBe(true);
    // Several paths: the per-path lines, so the unwatched one is findable.
    expect(summary?.title).toContain('/old.txt: not watched');
    expect(summary?.title).not.toContain('"/"');
  });

  test('one path reads as a plain line, without the path prefix', () => {
    expect(summarizeWatched([result()])?.title).toBe('watched — changes under / are recorded');
  });

  test('all paths unwatched reads as unwatched', () => {
    expect(
      summarizeWatched([result({ watched: false, reason: 'excluded', excluded_by: '/big' })])
        ?.watched,
    ).toBe(false);
  });

  test('a missing answer is unknown, never unwatched', () => {
    expect(summarizeWatched([undefined])).toBe(null);
    expect(summarizeWatched([result(), undefined])).toBe(null);
    expect(summarizeWatched([])).toBe(null);
  });
});