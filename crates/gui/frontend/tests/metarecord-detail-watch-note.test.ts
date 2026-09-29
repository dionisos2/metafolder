// metarecord-detail's watch note (spec-file-tracking "Watch check"): the
// verdict under the metarecord head — dim when watched, amber with the reason
// when not — and the reconcile button's label following the fetched answer
// rather than the record's raw `mf_watch` field (a record inheriting
// mf_watch = true carries no field of its own, yet is watched).

import { beforeEach, describe, expect, test } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/metarecord-detail');

const REPO = 'repo-1';

/** What POST /watch/activity answers, path → count (null = the endpoint fails). */
let activityCounts: Record<string, number> | null = null;
const UUID = 'aaa';

/** The shadow root the shell's mount path builds. */
function shadowFor(): ShadowRoot {
  const html = readFileSync(resolve(PANEL_DIR, 'index.html'), 'utf8');
  const doc = new DOMParser().parseFromString(html, 'text/html');
  const host = document.createElement('div');
  document.body.append(host);
  const shadow = host.attachShadow({ mode: 'open' });
  const body = document.createElement('div');
  body.className = 'mf-panel-body';
  for (const child of [...doc.body.childNodes]) {
    if (child.nodeName === 'SCRIPT' || child.nodeName === 'STYLE') continue;
    body.append(child);
  }
  shadow.append(body);
  return shadow;
}

/** One daemon watch/check result. */
function watchResult(overrides: Record<string, unknown> = {}) {
  return {
    path: '/notes.txt',
    watched: true,
    reason: 'watched',
    watched_dir: '/docs',
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

/**
 * Mounts the detail panel on one record at `rels`'s paths (empty = a pathless
 * record). `watchResults` answers POST /watch/check (null = the endpoint
 * fails, as on an older daemon); the record carries `mfWatchField` (bool) as
 * its own field.
 */
async function mountPanel(
  watchResults: Record<string, unknown>[] | null,
  mfWatchField = false,
  rels: string[] = ['/notes.txt'],
) {
  const noop = () => {};
  const store = new Map<string, unknown>([['selected_metarecord', { uuid: UUID, repo: REPO }]]);
  const fields = [
    {
      id: 1,
      name: 'mfr_path',
      value: { type: 'tree_ref', value: { parent: 'u-root', name: 'notes.txt' } },
    },
    ...(mfWatchField
      ? [{ id: 2, name: 'mf_watch', value: { type: 'bool', value: true } }]
      : []),
  ];
  const api = {
    ready: Promise.resolve(),
    workspaceId: 'ws-1',
    panelType: 'metarecord-detail',
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'token',
    pageSize: 100,
    settings: {},
    defaults: {},
    visible: true,
    onVisibility: noop,
    whenVisible: (fn: () => unknown) => void fn(),
    bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
    daemon: {
      query: async () => ({ uuids: [], nextCursor: null, total: 0 }),
      treePaths: async (_repo: string, _field: string, uuids: string[]) =>
        Object.fromEntries(uuids.map((uuid) => [uuid, rels])),
      metarecords: async () => new Map(),
      fields: async () => [],
      request: async () => ({ status: 200, body: null }),
      call: async (method: string, path: string, callBody: unknown) => {
        if (path.endsWith('/watch/activity')) {
          if (activityCounts === null) throw new Error('no such endpoint');
          const counts = activityCounts;
          return {
            since_ms: 0,
            total: counts[''] ?? 0,
            results: (callBody as { paths: string[] }).paths.map((p) => ({
              path: p,
              events: counts[p] ?? 0,
            })),
          };
        }
        const match = /\/metarecords\/([^/?]+)$/.exec(path);
        if (method === 'GET' && match)
          return { uuid: match[1], version: 1, fields: structuredClone(fields) };
        if (path.endsWith('/watch/check')) {
          if (watchResults === null) throw new Error('no such endpoint');
          return { results: watchResults };
        }
        return {};
      },
      parseQuery: async () => null,
      expandQuery: async () => '',
      resolvePath: async () => '',
      resolveTreeRef: async () => '',
      repoRoot: async () => '/tmp/repo',
      repoInternalDir: async () => '/tmp/repo/.metafolder/internal',
      metarecordPaths: async () => ['/tmp/repo/notes.txt'],
    },
    changes: { sync: async () => {}, subscribe: () => () => {} },
    query: { parse: async () => null, expand: async () => '', grammarSource: async () => '' },
    pick: { start: async () => '' },
    config: {
      pickerSeed: async () => null,
      refCompletionSeed: async () => null,
      refSeed: async () => null,
      labelSeparator: async () => ' | ',
    },
    recent: { touch: async () => {}, list: async () => [] },
    workspace: {
      get: async (key: string) => store.get(key) ?? null,
      set: async (key: string, value: unknown) => void store.set(key, value),
      adoptRepo: async () => {},
      onChange: noop,
    },
    commands: { register: async () => {}, invoke: () => null },
    addKeybinding: async () => null,
    fs: {
      readDir: async () => [],
      stat: async () => ({}),
      exists: async () => true,
      homeDir: async () => '/home/user',
    },
    trash: { list: async () => [], restore: async () => '', remove: async () => {}, empty: async () => 0 },
    history: { read: async () => [], append: async () => {} },
    statusBar: { message: async () => {}, error: async () => {} },
    messages: { list: async () => [], append: async () => {}, onAppend: noop },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  const mod = await import('../../default-config/panel-types/metarecord-detail/main.js');
  const shadow = shadowFor();
  await mod.mount(shadow, api as never);
  await new Promise((r) => setTimeout(r, 0));
  return {
    shadow,
    note: shadow.getElementById('watch-note') as HTMLElement,
    watchBtn: shadow.getElementById('watch-reconcile') as HTMLButtonElement,
    activity: shadow.getElementById('activity-note') as HTMLElement,
  };
}

describe('metarecord-detail watch note', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {};
    document.body.replaceChildren();
    activityCounts = null;
  });

  test('a not-watched record shows the reason in amber, and the watch button', async () => {
    const p = await mountPanel([
      watchResult({ watched: false, reason: 'excluded', excluded_by: '/big' }),
    ]);
    expect(p.note.hidden).toBe(false);
    expect(p.note.textContent).toContain('not watched');
    expect(p.note.textContent).toContain('inside /big');
    expect(p.note.classList.contains('unwatched')).toBe(true);
    expect(p.watchBtn.textContent).toBe('Watch and reconcile');
  });

  test('a record inheriting mf_watch reads as watched, without carrying the field', async () => {
    // No mf_watch field of its own: the raw-field fallback would have offered
    // "Watch and reconcile" for a watched record.
    const p = await mountPanel([watchResult()], false);
    expect(p.note.hidden).toBe(false);
    expect(p.note.textContent).toContain('watched — changes under /docs are recorded');
    expect(p.note.classList.contains('watched')).toBe(true);
    expect(p.watchBtn.textContent).toBe('Reconcile');
  });

  test('a failed check hides the note and falls back to the raw field', async () => {
    const p = await mountPanel(null, true);
    expect(p.note.hidden).toBe(true);
    // The fallback: the record's own mf_watch = true says watched.
    expect(p.watchBtn.textContent).toBe('Reconcile');
  });

  test('a pathless record shows nothing', async () => {
    // A pure metadata record carries no mfr_path: nothing to watch, and the
    // note says nothing rather than a misleading "not watched".
    const p = await mountPanel([watchResult()], false, []);
    expect(p.note.hidden).toBe(true);
  });

  // spec-gui "Watch activity": how many watcher events the record's file
  // received since the load, and its share of the repository's.
  test('the activity note counts the events at the record, amber on a hot spot', async () => {
    activityCounts = { '': 1000, '/notes.txt': 400 };
    const p = await mountPanel([watchResult()]);
    expect(p.activity.hidden).toBe(false);
    expect(p.activity.textContent).toContain('400 watcher event(s) since');
    expect(p.activity.textContent).toContain('40% of all events');
    expect(p.activity.classList.contains('hot')).toBe(true);
  });

  test('a quiet record, or an unanswered call, shows no activity note', async () => {
    activityCounts = { '': 1000 };
    expect((await mountPanel([watchResult()])).activity.hidden).toBe(true);
    activityCounts = null;
    expect((await mountPanel([watchResult()])).activity.hidden).toBe(true);
  });
});
