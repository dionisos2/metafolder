// The checked multi-selection survives changing the list (spec-gui "The checked
// selection"): rows checked in one list stay checked when the query moves to
// another, so a selection can be gathered across several lists — and rows
// checked elsewhere (the file manager, a script) light up here too. It used to
// be pruned to whatever the current query matched on every reset fetch, which
// threw away every check made in a previous list. The one thing that IS still
// dropped is a metarecord that no longer exists: a selection must never name
// what cannot be shown or unchecked any more.

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

type Handler = (...args: string[]) => unknown;

const record = (uuid: string, label: string) => ({
  uuid,
  version: 1,
  fields: [{ id: null, name: 'label', value: { type: 'string', value: label } }],
});

// What each query shows: the base list is A + B, the query "other" shows only C.
const LIST_AB = [record('u-a', 'A'), record('u-b', 'B')];
const LIST_C = [record('u-c', 'C')];

type Listener = (value: unknown, key?: string) => void;

function stub(vars: Record<string, unknown>, existing: Set<string>) {
  const noop = () => {};
  const handlers = new Map<string, Handler>();
  const store = new Map<string, unknown>([['active_repo', 'r'], ...Object.entries(vars)]);
  const listeners = new Map<string, Set<Listener>>();
  const writes: { key: string; value: unknown }[] = [];
  const notify = (key: string, value: unknown) => {
    for (const l of listeners.get(key) ?? []) l(value);
    for (const l of listeners.get('*') ?? []) l(value, key);
  };
  const statusBar = { message: vi.fn(async () => {}), error: vi.fn(async () => {}) };
  const api = {
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
      query: async (_repo: string, body: unknown) => {
              const query = (body as { query?: { text?: string } | null })?.query;
              const records = query?.text === 'other' ? LIST_C : LIST_AB;
              return { records, nextCursor: null, total: records.length };
            },
      treePaths: async (_repo: string, _field: string, uuids: string[]) =>
        Object.fromEntries(uuids.map((uuid) => [uuid, [] as string[]])),
      metarecords: async () => new Map(),
      fields: async () => [],
      request: async () => ({ status: 200, body: null }),
      call: async (method: string, path: string, body: unknown) => {
        if (method === 'POST' && path.endsWith('/query')) {
          // The pruning query: only the uuids that still exist come back.
          const query = (body as { query?: { type?: string; uuids?: string[] } })?.query;
          return { results: (query?.uuids ?? []).filter((u) => existing.has(u)) };
        }
        return {};
      },
      parseQuery: async () => null,
      expandQuery: async () => '',
      resolvePath: async () => '',
      resolveTreeRef: async () => '',
      repoRoot: async () => '/repo',
      repoInternalDir: async () => '/repo/.metafolder/internal',
      metarecordPaths: async () => [],
    },
    changes: { sync: async () => {}, subscribe: () => () => {} },
    query: {
      // The simplified → DSL expansion is the identity here; the parse keeps
      // the text so the stub cache can tell one list from the other.
      parse: async (dsl: string) => ({ text: dsl }),
      expand: async (text: string) => text,
      grammarSource: async () => '',
    },
    pick: { start: async () => '' },
    config: { pickerSeed: async () => null },
    workspace: {
      get: async (key: string) => store.get(key) ?? null,
      set: async (key: string, value: unknown) => {
        store.set(key, value);
        writes.push({ key, value });
        notify(key, value); // the backend echoes every write to the writer too
      },
      adoptRepo: async () => {},
      onChange: (key: string, listener: Listener) => {
        let set = listeners.get(key);
        if (!set) {
          set = new Set();
          listeners.set(key, set);
        }
        set.add(listener);
      },
    },
    commands: {
      register: (name: string, opts: { handler?: Handler }) => {
        if (opts.handler) handlers.set(name, opts.handler);
        return Promise.resolve(null);
      },
      invoke: () => null,
    },
    addKeybinding: async () => null,
    fs: {
      readDir: async () => [],
      stat: async () => ({}),
      exists: async () => true,
      homeDir: async () => '/home/user',
    },
    trash: { list: async () => [], restore: async () => '', remove: async () => {}, empty: async () => 0 },
    history: { read: async () => [], append: async () => {} },
    statusBar,
    messages: { list: async () => [], append: async () => {}, onAppend: noop },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  return {
    api,
    handlers,
    store,
    statusBar,
    selected: () => (store.get('selected_metarecords') as string[] | undefined) ?? [],
    /** A change made by another panel or a script. */
    externalSet(key: string, value: unknown) {
      store.set(key, value);
      notify(key, value);
    },
  };
}

/** Mounts the list, letting its async start settle. */
async function mountList(vars: Record<string, unknown>, existing = new Set(['u-a', 'u-b', 'u-c'])) {
  const harness = stub(vars, existing);
  const shadow = shadowFor();
  const mod = await import('../../default-config/panel-types/metarecord-list/main.js');
  await mod.mount(shadow, harness.api as never);
  await settle();
  return { ...harness, shadow };
}

/** A few event-loop turns: fetch → prepare → render is several awaits deep. */
async function settle() {
  for (let i = 0; i < 5; i++) await new Promise((r) => setTimeout(r, 0));
}

/** The rendered rows, in order. */
function rows(shadow: ShadowRoot): Element[] {
  return [...shadow.querySelectorAll('#rows tr.row')];
}

describe('the checked selection across lists', () => {
  beforeEach(() => {
    // jsdom does not implement scrollIntoView, which setCursor() calls.
    Element.prototype.scrollIntoView = () => {};
  });

  test('checks made in one list survive moving to another list', async () => {
    const harness = await mountList({});
    // Check A and B (the cursor starts on row 0; then move to row 1).
    await harness.handlers.get('metarecord-list:select')!('toggle');
    await harness.handlers.get('metarecord-list:next')!();
    await harness.handlers.get('metarecord-list:select')!('toggle');
    expect(harness.selected()).toEqual(['u-a', 'u-b']);

    // Change the list: the query "other" shows only C, so A and B no longer
    // match — but they were checked and stay checked.
    const queryInput = harness.shadow.querySelector('#query-input') as HTMLInputElement;
    queryInput.value = 'other';
    await harness.handlers.get('metarecord-list:apply')!('simplified');
    await settle();
    expect(harness.selected()).toEqual(['u-a', 'u-b']);
    // The new list is on screen, its own row not checked.
    expect(rows(harness.shadow).map((row) => row.classList.contains('checked'))).toEqual([false]);
  });

  test('select all keeps what another list already checked', async () => {
    const { handlers, selected } = await mountList(
      { selected_metarecords: ['u-x'] },
      new Set(['u-a', 'u-b', 'u-c', 'u-x']),
    );
    await handlers.get('metarecord-list:select')!('all');
    expect(selected()).toEqual(['u-x', 'u-a', 'u-b']);
  });

  test('a metarecord that no longer exists is dropped from the selection', async () => {
    const { selected } = await mountList(
      { selected_metarecords: ['u-a', 'u-gone'] },
      new Set(['u-a', 'u-b', 'u-c']), // u-gone was deleted elsewhere
    );
    expect(selected()).toEqual(['u-a']);
  });

  test('checks made elsewhere light up the rows of this list', async () => {
    const harness = await mountList({});
    harness.externalSet('selected_metarecords', ['u-b']);
    const checked = rows(harness.shadow).map((row) => row.classList.contains('checked'));
    expect(checked).toEqual([false, true]);
  });

  test('select none clears the whole selection', async () => {
    const { handlers, selected } = await mountList({ selected_metarecords: ['u-a', 'u-z'] });
    await handlers.get('metarecord-list:select')!('none');
    expect(selected()).toEqual([]);
  });
});
