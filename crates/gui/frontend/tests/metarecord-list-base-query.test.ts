// The base query the list publishes for the panels that act on "the current
// query" (metarecord-detail's bulk commands).
//
// Regression: they used to read `metarecord-list:query`, which holds the
// *simplified* text, and hand it to the NORMAL DSL parser. A simplified query
// like `#jazz` either failed to parse or — worse — meant something else, so a
// bulk edit could target a set the user never asked for. The list now publishes
// the expanded, authoritative DSL as `metarecord-list:base-query-text`.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/metarecord-list');

/** The shell's mount path: the panel's body (minus scripts/styles) into a Shadow root. */
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

type Call = { method: string; path: string; body: unknown };

function stubApi(vars: Record<string, unknown>, calls: Call[]) {
  const noop = () => {};
  const store = new Map<string, unknown>([['active_repo', 'r'], ...Object.entries(vars)]);
  const listeners = new Map<string, (value: unknown) => void>();
  const statusBar = { message: vi.fn(async () => {}), error: vi.fn(async () => {}) };
  return {
    api: {
      ready: Promise.resolve(),
      workspaceId: 'ws-1',
      panelType: 'metarecord-list',
      guiServer: 'http://127.0.0.1:7524',
      sessionToken: 'token',
      pageSize: 100,
      settings: {},
      defaults: {},
      visible: true,
      onVisibility: noop,
      whenVisible: (fn: () => void) => fn(),
      bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
      daemon: {
        request: async () => ({ status: 200, body: null }),
        call: async (method: string, path: string, body: unknown) => {
          calls.push({ method, path, body });
          return { results: [], next_cursor: null };
        },
        parseQuery: async () => null,
        expandQuery: async () => '',
        resolvePath: async () => '',
        resolveTreeRef: async () => '',
        invalidatePath: () => true,
        repoRoot: async () => '/repo',
        repoInternalDir: async () => '/repo/.metafolder/internal',
        metarecordPaths: async () => [],
      },
      cache: {
        query: async (repo: string, body: unknown) => {
          calls.push({ method: 'QUERY', path: `/repos/${repo}/query`, body });
          return { records: [], nextCursor: null, total: 0 };
        },
        fetchMetarecords: async () => {},
        fetchTreeRefs: async () => {},
        fetchFields: async () => {},
        readMetarecord: () => null,
        readTreeRef: () => [],
        readFields: () => [],
        fieldType: () => null,
        sync: async () => {},
        subscribe: () => () => {},
        REFRESH: Symbol('refresh'),
      },
      // The DSL is echoed back as the "parsed" IR, so a query call shows which
      // text was compiled and run.
      query: {
        parse: async (dsl: string) => ({ dsl }),
        expand: async (text: string) => text,
        grammarSource: async () => '',
      },
      pick: { start: async () => '' },
      config: { pickerSeed: async () => null },
      workspace: {
        get: async (key: string) => store.get(key) ?? null,
        set: vi.fn(async (key: string, value: unknown) => void store.set(key, value)),
        adoptRepo: async () => {},
        onChange: (key: string, fn: (value: unknown) => void) => void listeners.set(key, fn),
      },
      commands: { register: async () => null, invoke: () => null },
      addKeybinding: async () => null,
      fs: { readDir: async () => [], stat: async () => ({}), homeDir: async () => '/home/user' },
      trash: { list: async () => [], restore: async () => '', remove: async () => {}, empty: async () => 0 },
      history: { read: async () => [], append: async () => {} },
      statusBar,
      messages: { list: async () => [], append: async () => {}, onAppend: noop },
      contextMenu: Object.assign(noop, { addDefaultItems: noop }),
    },
    store,
    listeners,
  };
}

/** Mounts the list with a custom simplified→normal expansion, and records every
 *  workspace variable the panel published. */
async function mountWithExpansion(
  vars: Record<string, unknown>,
  expand: (text: string) => string,
) {
  const shadow = shadowFor();
  const calls: Call[] = [];
  const { api } = stubApi(vars, calls);
  api.query.expand = async (text: string) => expand(text);
  const sets = new Map<string, unknown>();
  const inner = api.workspace.set;
  api.workspace.set = vi.fn(async (key: string, value: unknown) => {
    sets.set(key, value);
    return inner(key, value);
  });
  const mod = await import('../../default-config/panel-types/metarecord-list/main.js');
  await mod.mount(shadow, api as never);
  await new Promise((r) => setTimeout(r, 0));
  return { sets, calls };
}

describe('the published base query', () => {
  beforeEach(() => {
    document.body.innerHTML = '';
    vi.resetModules();
  });

  test('is the EXPANDED normal DSL, never the simplified text', async () => {
    const simplified = '#jazz';
    const normal = 'tag ->* (mf_schema = "tag" AND path = "jazz")';
    const { sets } = await mountWithExpansion({ 'metarecord-list:query': simplified }, (text) =>
      text === simplified ? normal : text,
    );
    expect(sets.get('metarecord-list:base-query-text')).toBe(normal);
    expect(sets.get('metarecord-list:base-query-text')).not.toBe(simplified);
  });

  test('is empty when the query is empty, which reads as "every metarecord"', async () => {
    const { sets } = await mountWithExpansion({}, (text) => text);
    expect(sets.get('metarecord-list:base-query-text')).toBe('');
  });
});
