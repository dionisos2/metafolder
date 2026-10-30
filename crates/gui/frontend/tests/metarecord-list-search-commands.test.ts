// metarecord-list search-field editing commands (GUI: search commands).
//
// Mounts the real panel against its own markup (like panel-mount.test.ts) with
// a recording `commands.register`, then drives the new commands and checks
// their DOM effects: editing/clearing the three search fields (finder,
// simplified query, normal DSL) and the Enter-leaves / Shift+Enter-stays
// behaviour of the field inputs.

import { describe, expect, test, vi, beforeEach } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { withRuns } from './support/runs';

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

type Handler = (arg?: unknown) => unknown;

/** A stub API that records registered command handlers and a workspace store. */
function stubApi(handlers: Map<string, Handler>, store: Map<string, unknown>) {
  const noop = () => {};
  return {
    ready: Promise.resolve(),
    workspaceId: 'ws-1',
    panelType: 'metarecord-list',
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'token',
    pageSize: 100,
    settings: {},
    defaults: {},
    visible: false,
    onVisibility: noop,
    whenVisible: noop, // keep start() from running (no eager fetch)
    bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
    daemon: {
      query: async () => ({ records: [], nextCursor: null, total: 0 }),
      treePaths: async (_repo: string, _field: string, uuids: string[]) =>
        Object.fromEntries(uuids.map((uuid) => [uuid, [] as string[]])),
      metarecords: async () => new Map(),
      fields: async () => [],
      request: async () => ({ status: 200, body: null }),
      call: async () => null,
      resolvePath: async () => '',
      resolveTreeRef: async () => '',
      repoRoot: async () => '/tmp/repo',
      repoInternalDir: async () => '/tmp/repo/.metafolder/internal',
      metarecordPaths: async () => [],
    },
    changes: { sync: async () => {}, subscribe: () => () => {} },
    query: { parse: async () => null, expand: async () => '', grammarSource: async () => '' },
    pick: { start: async () => '' },
    config: {},
    workspace: {
      get: async (key: string) => store.get(key) ?? null,
      set: async (key: string, value: unknown) => void store.set(key, value),
      adoptRepo: async () => {},
      onChange: noop,
    },
    provide: vi.fn(),
    commands: {
      register: async (name: string, spec: { handler: Handler }) => {
        handlers.set(name, spec.handler);
        return null;
      },
      invoke: () => null,
    },
    addKeybinding: async () => null,
    fs: { readDir: async () => [], stat: async () => ({}), exists: async () => true, homeDir: async () => '/home/user' },
    trash: { list: async () => [], restore: async () => '', remove: async () => {}, empty: async () => 0 },
    history: { read: async () => [], append: async () => {} },
    statusBar: { message: async () => {}, error: async () => {} },
    messages: { list: async () => [], append: async () => {}, onAppend: noop },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
}

async function mountPanel(initial: Record<string, unknown> = {}) {
  const shadow = shadowFor();
  const handlers = new Map<string, Handler>();
  // The workspace store the stub writes to: `vars` is how a test reads back
  // what a command persisted (e.g. `metarecord-list:query`).
  const vars = new Map<string, unknown>(Object.entries(initial));
  const mod = await import('../../default-config/panel-types/metarecord-list/main.js');
  await mod.mount(shadow, withRuns(stubApi(handlers, vars)) as never);
  const el = <T extends HTMLElement = HTMLInputElement>(id: string) =>
    shadow.getElementById(id) as unknown as T;
  return {
    shadow,
    handlers,
    vars,
    finder: el('finder-input'),
    query: el('query-input'),
    normal: el('normal-input'),
    normalEditor: el<HTMLElement>('normal-editor'),
    expansion: el('expansion-output'),
    expansionRow: el<HTMLElement>('expansion-row'),
    columns: el('columns-input'),
    invoke: async (name: string, ...args: unknown[]) => {
      const h = handlers.get(name);
      if (!h) throw new Error(`command not registered: ${name}`);
      await h(...args);
    },
  };
}

describe('zone commands', () => {
  beforeEach(() => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('[]', { status: 200 })),
    );
    document.body.replaceChildren();
  });

  test('the normal DSL editor is always shown, with no button to hide it', async () => {
    const p = await mountPanel();
    expect(p.normalEditor.hidden).toBe(false);
    expect(p.shadow.getElementById('normal-toggle')).toBeNull();
    // Always editable: it is AND-ed with the simplified query, never a mirror.
    expect(p.normal.readOnly).toBe(false);
    expect(p.shadow.getElementById('normal-freeze')).toBeNull();
  });

  test('a stale stored preference to hide it is ignored', async () => {
    const p = await mountPanel({ 'metarecord-list:normal-shown': false });
    expect(p.normalEditor.hidden).toBe(false);
  });

  test('there is no toggle normal any more', async () => {
    const p = await mountPanel();
    await expect(p.invoke('metarecord-list:toggle', 'normal')).rejects.toThrow(/unknown flag/);
  });

  test('every text zone starts with an icon, and the order checkbox sits at the right', async () => {
    const p = await mountPanel();
    for (const id of ['finder-input', 'query-input', 'normal-input', 'columns-input', 'order-field']) {
      const row = p.shadow.getElementById(id)!.parentElement!;
      expect(row.firstElementChild?.classList.contains('zone-icon'), id).toBe(true);
      expect(row.firstElementChild?.nextElementSibling?.id, id).toBe(id);
    }
    const order = p.shadow.getElementById('order-field')!;
    const box = p.shadow.getElementById('order-on')!;
    expect(order.compareDocumentPosition(box) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  // ── focus ───────────────────────────────────────────────────────────────

  test('focus normal and focus simplified move the focus, keeping the other zone', async () => {
    // Both zones are part of the query: going to one leaves the other as it is.
    const p = await mountPanel();
    p.query.value = 'jazz';
    p.normal.value = 'rating > 3';

    await p.invoke('metarecord-list:focus', 'normal');
    expect(p.shadow.activeElement).toBe(p.normal);
    await p.invoke('metarecord-list:focus', 'simplified');
    expect(p.shadow.activeElement).toBe(p.query);

    expect(p.query.value).toBe('jazz');
    expect(p.normal.value).toBe('rating > 3');
  });

  test('focus finder and focus columns move the focus', async () => {
    const p = await mountPanel();
    await p.invoke('metarecord-list:focus', 'finder');
    expect(p.shadow.activeElement).toBe(p.finder);
    await p.invoke('metarecord-list:focus', 'columns');
    expect(p.shadow.activeElement).toBe(p.columns);
  });

  test('an unknown zone is an error, not a silent no-op', async () => {
    const p = await mountPanel();
    await expect(p.invoke('metarecord-list:focus', 'nope')).rejects.toThrow(/unknown zone/);
  });

  // ── clear ───────────────────────────────────────────────────────────────

  test('clear all empties the three search fields', async () => {
    const p = await mountPanel();
    p.finder.value = 'foo';
    p.query.value = 'rating>3';
    p.normal.value = 'rating gt 3';

    await p.invoke('metarecord-list:clear-all');

    expect(p.finder.value).toBe('');
    expect(p.query.value).toBe('');
    expect(p.normal.value).toBe('');
  });

  test('focus simplified clear empties it and its expansion, keeps the normal DSL, and focuses it', async () => {
    const p = await mountPanel();
    p.query.value = 'rating>3';
    p.expansion.value = 'rating > 3';
    p.normal.value = 'x = 1';

    await p.invoke('metarecord-list:focus', 'simplified', 'clear');

    expect(p.query.value).toBe('');
    expect(p.expansion.value).toBe('');
    expect(p.normal.value).toBe('x = 1');
    expect(p.shadow.activeElement).toBe(p.query);
  });

  test('focus finder clear empties the finder and focuses it', async () => {
    const p = await mountPanel();
    p.finder.value = 'foo';

    await p.invoke('metarecord-list:focus', 'finder', 'clear');

    expect(p.finder.value).toBe('');
    expect(p.shadow.activeElement).toBe(p.finder);
  });

  test('focus normal clear empties the normal DSL, keeps the simplified query, and focuses it', async () => {
    const p = await mountPanel();
    p.query.value = 'jazz';
    p.normal.value = 'rating gt 3';

    await p.invoke('metarecord-list:focus', 'normal', 'clear');

    expect(p.query.value).toBe('jazz');
    expect(p.normal.value).toBe('');
    expect(p.shadow.activeElement).toBe(p.normal);
  });

  test('focus columns clear empties the columns field — the zone that had no clear', async () => {
    const p = await mountPanel();
    p.columns.value = 'name rating';

    await p.invoke('metarecord-list:focus', 'columns', 'clear');

    expect(p.columns.value).toBe('');
    expect(p.shadow.activeElement).toBe(p.columns);
  });

  // ── apply ───────────────────────────────────────────────────────────────

  test('Enter in the simplified field leaves it (blur); Shift+Enter keeps focus', async () => {
    const p = await mountPanel();

    p.query.focus();
    expect(p.shadow.activeElement).toBe(p.query);
    p.query.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    expect(p.shadow.activeElement).not.toBe(p.query);

    p.query.focus();
    p.query.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Enter', shiftKey: true, bubbles: true }),
    );
    expect(p.shadow.activeElement).toBe(p.query);
  });

  test('apply leaves the zone; `stay` keeps the focus in it', async () => {
    // The finder is the one zone whose Enter is a keybinding rather than a
    // hard-coded handler, so both behaviours have to survive as commands —
    // as one verb with a modifier, not two verbs.
    const p = await mountPanel();

    p.finder.focus();
    await p.invoke('metarecord-list:apply', 'finder', 'stay');
    expect(p.shadow.activeElement).toBe(p.finder);

    await p.invoke('metarecord-list:apply', 'finder');
    expect(p.shadow.activeElement).not.toBe(p.finder);
  });

  test('apply on an unfocused zone does not steal or drop the focus', async () => {
    const p = await mountPanel();
    p.query.focus();

    await p.invoke('metarecord-list:apply', 'columns');

    expect(p.shadow.activeElement).toBe(p.query);
  });

  // ── insert ──────────────────────────────────────────────────────────────

  test('insert splices at the caret and leaves the caret after the text', async () => {
    const p = await mountPanel();
    p.query.value = 'ab';
    p.query.focus();
    p.query.setSelectionRange(1, 1);

    await p.invoke('metarecord-list:insert', 'simplified', '#=jazz');

    expect(p.query.value).toBe('a#=jazzb');
    expect(p.query.selectionStart).toBe(7);
  });

  test('insert replaces the selection', async () => {
    const p = await mountPanel();
    p.query.value = 'old text';
    p.query.focus();
    p.query.setSelectionRange(0, 3);

    await p.invoke('metarecord-list:insert', 'simplified', 'new');

    expect(p.query.value).toBe('new text');
  });

  test('insert into the simplified zone keeps the normal DSL: the two are AND-ed', async () => {
    // What `tag:query` relies on to add a filter to a query a command wrote.
    const p = await mountPanel();
    p.normal.value = 'mfr_path -> "/music"';

    await p.invoke('metarecord-list:insert', 'simplified', '#=jazz');

    expect(p.query.value).toBe('#=jazz');
    expect(p.normal.value).toBe('mfr_path -> "/music"');
  });

  test('insert fires a bubbling input event so the live preview refreshes', async () => {
    // Assigning `.value` fires nothing, so the debounced expand(A) -> B mirror
    // would keep showing a stale expansion (panel-shim/history.js does the same).
    const p = await mountPanel();
    const seen: string[] = [];
    p.query.addEventListener('input', () => seen.push(p.query.value));

    await p.invoke('metarecord-list:insert', 'simplified', 'rating>3');

    expect(seen).toEqual(['rating>3']);
  });

  test('insert does not run the query — it prepares it', async () => {
    const p = await mountPanel();
    await p.invoke('metarecord-list:insert', 'simplified', 'rating>3');
    expect(p.query.value).toBe('rating>3');
    // Nothing was persisted: only `apply` commits.
    expect(p.shadow.activeElement).toBe(p.query);
  });

  test('insert `stay` leaves the focus where it is', async () => {
    // A caller that writes and then runs the query must not yank the caret
    // out of what the user is doing — `tag:query` composes this.
    const p = await mountPanel();
    p.finder.focus();

    await p.invoke('metarecord-list:insert', 'simplified', '#=jazz', 'stay');

    expect(p.query.value).toBe('#=jazz');
    expect(p.query.selectionStart).toBe('#=jazz'.length);
    expect(p.shadow.activeElement).toBe(p.finder);
  });

  test('insert `stay` still refreshes the expansion preview', async () => {
    // Focused or not, the expansion line must follow what was inserted.
    const p = await mountPanel();
    p.normal.focus();
    const seen: string[] = [];
    p.query.addEventListener('input', () => seen.push(p.query.value));

    await p.invoke('metarecord-list:insert', 'simplified', '#=jazz', 'stay');

    expect(seen).toEqual(['#=jazz']);
    expect(p.shadow.activeElement).toBe(p.normal);
  });

  test('a trailing run that is not `stay` is more text, not a modifier', async () => {
    // The text is free text and holds spaces unquoted; `stay` is told apart
    // by being spelled exactly that.
    const p = await mountPanel();

    await p.invoke('metarecord-list:insert', 'simplified', '#=jazz', 'rock');

    expect(p.query.value).toBe('#=jazz rock');
    expect(p.shadow.activeElement).toBe(p.query);
  });

  test('insert `stay` then apply runs the search without touching the focus', async () => {
    // The pair `tag:query` is built from: the filter lands, the query
    // runs (`apply` persists it), and the focus is never in the query zone
    // along the way.
    const p = await mountPanel();
    p.finder.focus();

    await p.invoke('metarecord-list:insert', 'simplified', '#=jazz', 'stay');
    await p.invoke('metarecord-list:apply', 'simplified');

    expect(p.vars.get('metarecord-list:query')).toBe('#=jazz');
    expect(p.shadow.activeElement).toBe(p.finder);
  });
});

describe('display mode', () => {
  beforeEach(() => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response('[]', { status: 200 })));
    document.body.replaceChildren();
  });
  const isGrid = (shadow: ShadowRoot) =>
    shadow.querySelector('.mf-panel-body')!.classList.contains('grid');

  test('the mode is a workspace variable, like the other view settings', async () => {
    const p = await mountPanel();
    await p.invoke('metarecord-list:set', 'mode', 'grid');
    expect(p.vars.get('metarecord-list:mode')).toBe('grid');
    expect(isGrid(p.shadow)).toBe(true);
  });

  test('a stored mode is what the panel opens in', async () => {
    const p = await mountPanel({ 'metarecord-list:mode': 'grid' });
    expect(isGrid(p.shadow)).toBe(true);
  });

  test('a stored value that is no mode falls back to the table', async () => {
    const p = await mountPanel({ 'metarecord-list:mode': 'mosaic' });
    expect(isGrid(p.shadow)).toBe(false);
  });

  test('there is no toggle freeze any more', async () => {
    const p = await mountPanel();
    await expect(p.invoke('metarecord-list:toggle', 'freeze')).rejects.toThrow(/unknown flag/);
  });

  test('toggle expansion hides and shows the expansion line, and is persisted', async () => {
    const p = await mountPanel();
    p.normal.focus();
    expect(p.expansionRow.hidden).toBe(false);

    await p.invoke('metarecord-list:toggle', 'expansion');
    expect(p.expansionRow.hidden).toBe(true);
    expect(p.vars.get('metarecord-list:show-expansion')).toBe(false);

    await p.invoke('metarecord-list:toggle', 'expansion');
    expect(p.expansionRow.hidden).toBe(false);
    expect(p.vars.get('metarecord-list:show-expansion')).toBe(true);
    expect(p.shadow.activeElement).toBe(p.normal);
  });

  test('toggle mode flips between the table and the grid, and is persisted', async () => {
    const p = await mountPanel();
    await p.invoke('metarecord-list:toggle', 'mode');
    expect(isGrid(p.shadow)).toBe(true);
    expect(p.vars.get('metarecord-list:mode')).toBe('grid');
    await p.invoke('metarecord-list:toggle', 'mode');
    expect(isGrid(p.shadow)).toBe(false);
    expect(p.vars.get('metarecord-list:mode')).toBe('table');
  });
});
