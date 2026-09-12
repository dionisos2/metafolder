// duplicates panel: choosing which copy survives (spec-duplicates "GUI").
// Selecting a group row publishes the *group* metarecord, so the detail panel
// can show what the row summarises; "keep this one" trashes the other copies of
// the group under the cursor; and both trash actions re-count the group on the
// spot — dissolving it when a single copy is left, exactly as the daemon does.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve as resolvePath } from 'node:path';

const PANEL_DIR = resolvePath(process.cwd(), '../default-config/panel-types/duplicates');

/** The shell's mount path: the panel's body (minus scripts/styles) into a Shadow root. */
function shadowFor(): ShadowRoot {
  const html = readFileSync(resolvePath(PANEL_DIR, 'index.html'), 'utf8');
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
type Call = { method: string; path: string; body: unknown };

const int = (value: number): Metafolder.Value => ({ type: 'int', value });
const str = (value: string): Metafolder.Value => ({ type: 'string', value });

/** A group as the panel's own query selects it. */
function group(uuid: string, size: number, count: number, reclaimable: number) {
  return {
    uuid,
    fields: [
      { name: 'mfr_content_hash', value: str(`${uuid}-hash`) },
      { name: 'mfr_content_size', value: int(size) },
      { name: 'mfr_duplicate_count', value: int(count) },
      { name: 'mfr_duplicate_reclaimable', value: int(reclaimable) },
    ],
  };
}

/** A member: its path (resolved separately) and its inode when hard-linked. */
type Member = { uuid: string; path: string; inode?: string };

function daemonStub(calls: Call[], groups: unknown[], members: Member[], failLoad = false) {
  return async (method: string, path: string, body: unknown) => {
    calls.push({ method, path, body });
    if (failLoad) throw new Error('daemon is down');
    if (path.endsWith('/query/fields/resolve-tree')) {
      return Object.fromEntries(members.map((m) => [m.uuid, [m.path]]));
    }
    const query = (body as { query?: { field?: string } })?.query;
    if (query?.field === 'mfr_duplicate_group') {
      return {
        results: members.map((m) => ({
          uuid: m.uuid,
          fields: m.inode ? [{ name: 'mfr_inode', value: str(m.inode) }] : [],
        })),
      };
    }
    return { results: groups };
  };
}

type Options = {
  /** No active repository (the panel has nothing to show). */
  noRepo?: boolean;
  /** Every daemon call fails. */
  failLoad?: boolean;
  /** Paths whose trashing fails, by absolute path. */
  trashFails?: string[];
};

function stubApi(
  handlers: Map<string, Handler>,
  calls: Call[],
  groups: unknown[],
  members: Member[],
  options: Options = {},
) {
  const noop = () => {};
  const store = new Map<string, unknown>(options.noRepo ? [] : [['active_repo', 'r']]);
  const statusBar = { message: vi.fn(async () => {}), error: vi.fn(async () => {}) };
  const setVar = vi.fn(async (key: string, value: unknown) => void store.set(key, value));
  const trashPath = vi.fn(async (_repo: string, path: string) => {
    if (options.trashFails?.includes(path)) throw new Error(`cannot trash ${path}`);
    return 'trash-id';
  });
  return {
    api: {
      ready: Promise.resolve(),
      workspaceId: 'ws-1',
      panelType: 'duplicates',
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
        call: daemonStub(calls, groups, members, options.failLoad),
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
        query: async () => ({ records: [], nextCursor: null, total: 0 }),
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
      query: { parse: async () => null, expand: async () => '', grammarSource: async () => '' },
      pick: { start: async () => '' },
      config: { pickerSeed: async () => null },
      workspace: {
        get: async (key: string) => store.get(key) ?? null,
        set: setVar,
        adoptRepo: async () => {},
        onChange: noop,
      },
      commands: {
        register: async (name: string, spec: { handler: Handler }) => {
          handlers.set(name, spec.handler);
          return null;
        },
        invoke: () => null,
      },
      addKeybinding: async () => null,
      fs: { readDir: async () => [], stat: async () => ({}), homeDir: async () => '/home/user' },
      trash: {
        list: async () => [],
        restore: async () => '',
        remove: async () => {},
        empty: async () => 0,
        trashPath,
      },
      history: { read: async () => [], append: async () => {} },
      statusBar,
      messages: { list: async () => [], append: async () => {}, onAppend: noop },
      contextMenu: Object.assign(noop, { addDefaultItems: noop }),
    },
    statusBar,
    setVar,
    trashPath,
  };
}

async function mountPanel(groups: unknown[], members: Member[], options: Options = {}) {
  const shadow = shadowFor();
  const handlers = new Map<string, Handler>();
  const calls: Call[] = [];
  const { api, statusBar, setVar, trashPath } = stubApi(handlers, calls, groups, members, options);
  const mod = await import('../../default-config/panel-types/duplicates/main.js');
  // `mount` is synchronous here and returns the panel's cleanup, not a promise.
  mod.mount(shadow, api as never);
  await new Promise((r) => setTimeout(r, 0)); // let the deferred start settle
  const invoke = async (name: string) => {
    const h = handlers.get(name);
    if (!h) throw new Error(`command not registered: ${name}`);
    await h();
  };
  return {
    shadow,
    calls,
    statusBar,
    setVar,
    trashPath,
    invoke,
    rows: () => [...shadow.querySelectorAll('#entries li')] as HTMLElement[],
    cursorRow: () => shadow.querySelector<HTMLElement>('#entries li.cursor'),
    placeholder: () => shadow.getElementById('placeholder') as HTMLElement,
    status: () => shadow.getElementById('status-line') as HTMLElement,
    /** The visible text of the group rows, "<reclaimable> <size> <count>". */
    groupRows: () =>
      ([...shadow.querySelectorAll('#entries li:not(.member)')] as HTMLElement[]).map((li) =>
        [...li.children].map((c) => c.textContent).join(' '),
      ),
  };
}

describe('duplicates panel', () => {
  beforeEach(() => {
    vi.stubGlobal('confirm', vi.fn(() => true));
    document.body.replaceChildren();
  });

  test('clicking a group row publishes the group metarecord as the selection', async () => {
    const p = await mountPanel([group('g1', 10, 3, 20)], []);
    p.rows()[0].dispatchEvent(new MouseEvent('click', { bubbles: true }));
    await new Promise((r) => setTimeout(r, 0));
    expect(p.setVar).toHaveBeenCalledWith('selected_metarecord', { uuid: 'g1', repo: 'r' });
  });

  test('clicking a member row still publishes the member', async () => {
    const members = [
      { uuid: 'm1', path: '/a.bin' },
      { uuid: 'm2', path: '/b.bin' },
    ];
    const p = await mountPanel([group('g1', 10, 2, 10)], members);
    await p.invoke('duplicates:next'); // onto the group row
    await p.invoke('duplicates:toggle'); // expand it
    await new Promise((r) => setTimeout(r, 0));
    p.rows()[1].dispatchEvent(new MouseEvent('click', { bubbles: true }));
    await new Promise((r) => setTimeout(r, 0));
    expect(p.setVar).toHaveBeenCalledWith('selected_metarecord', { uuid: 'm1', repo: 'r' });
  });

  test('keep-this trashes the other copies and the group goes with them', async () => {
    const members = [
      { uuid: 'm1', path: '/keep.bin' },
      { uuid: 'm2', path: '/copy1.bin' },
      { uuid: 'm3', path: '/copy2.bin' },
    ];
    const p = await mountPanel([group('g1', 10, 3, 20)], members);
    await p.invoke('duplicates:next');
    await p.invoke('duplicates:toggle');
    await new Promise((r) => setTimeout(r, 0));
    await p.invoke('duplicates:next'); // onto the first member, the one to keep
    await p.invoke('duplicates:keep');

    expect(p.trashPath.mock.calls.map((c) => c[1])).toEqual([
      '/repo/copy1.bin',
      '/repo/copy2.bin',
    ]);
    expect(p.rows()).toHaveLength(0); // one copy left: the group is dissolved
  });

  test('keep-this on a group row asks for a copy instead of trashing anything', async () => {
    const members = [
      { uuid: 'm1', path: '/a.bin' },
      { uuid: 'm2', path: '/b.bin' },
    ];
    const p = await mountPanel([group('g1', 10, 2, 10)], members);
    await p.invoke('duplicates:next'); // the cursor sits on the group row
    await p.invoke('duplicates:keep');
    expect(p.trashPath).not.toHaveBeenCalled();
    expect(p.statusBar.message).toHaveBeenCalled();
  });

  test('trashing one member of three re-counts the group in place', async () => {
    const members = [
      { uuid: 'm1', path: '/a.bin' },
      { uuid: 'm2', path: '/b.bin' },
      { uuid: 'm3', path: '/c.bin' },
    ];
    const p = await mountPanel([group('g1', 10, 3, 20)], members);
    await p.invoke('duplicates:next');
    await p.invoke('duplicates:toggle');
    await new Promise((r) => setTimeout(r, 0));
    await p.invoke('duplicates:next');
    await p.invoke('duplicates:trash');

    expect(p.trashPath.mock.calls.map((c) => c[1])).toEqual(['/repo/a.bin']);
    expect(p.groupRows()[0]).toContain(' 2 '); // the count, one lower
    expect(p.groupRows()[0]).toContain('10B'); // and one copy less to reclaim
    expect(p.rows().filter((li) => li.classList.contains('member'))).toHaveLength(2);
  });

  test('trashing one of a pair dissolves the group', async () => {
    const members = [
      { uuid: 'm1', path: '/a.bin' },
      { uuid: 'm2', path: '/b.bin' },
    ];
    const p = await mountPanel([group('g1', 10, 2, 10)], members);
    await p.invoke('duplicates:next');
    await p.invoke('duplicates:toggle');
    await new Promise((r) => setTimeout(r, 0));
    await p.invoke('duplicates:next');
    await p.invoke('duplicates:trash');
    expect(p.rows()).toHaveLength(0);
  });

  test('a declined confirmation trashes nothing', async () => {
    vi.stubGlobal('confirm', vi.fn(() => false));
    const members = [
      { uuid: 'm1', path: '/a.bin' },
      { uuid: 'm2', path: '/b.bin' },
    ];
    const p = await mountPanel([group('g1', 10, 2, 10)], members);
    await p.invoke('duplicates:next');
    await p.invoke('duplicates:toggle');
    await new Promise((r) => setTimeout(r, 0));
    await p.invoke('duplicates:next');
    await p.invoke('duplicates:keep');
    expect(p.trashPath).not.toHaveBeenCalled();
    expect(p.rows()).toHaveLength(3);
  });
});

describe('duplicates panel, the harder paths', () => {
  beforeEach(() => {
    vi.stubGlobal('confirm', vi.fn(() => true));
    document.body.replaceChildren();
  });

  test('no active repository: the panel says so and asks the daemon nothing', async () => {
    const p = await mountPanel([group('g1', 10, 2, 10)], [], { noRepo: true });
    expect(p.placeholder().textContent).toContain('No active repository');
    expect(p.calls).toHaveLength(0);
  });

  test('no groups: the placeholder points at the scan', async () => {
    const p = await mountPanel([], []);
    expect(p.placeholder().textContent).toContain('run a scan');
    expect(p.status().textContent).toBe('');
  });

  test('a daemon failure is reported, not swallowed', async () => {
    const p = await mountPanel([group('g1', 10, 2, 10)], [], { failLoad: true });
    expect(p.statusBar.error).toHaveBeenCalled();
    expect(p.rows()).toHaveLength(0);
  });

  test('a refresh keeps the open group open and the cursor on its copy', async () => {
    const members = [
      { uuid: 'm1', path: '/a.bin' },
      { uuid: 'm2', path: '/b.bin' },
    ];
    const p = await mountPanel([group('g1', 10, 2, 10)], members);
    await p.invoke('duplicates:next');
    await p.invoke('duplicates:toggle');
    await new Promise((r) => setTimeout(r, 0));
    await p.invoke('duplicates:next'); // onto /a.bin
    await p.invoke('duplicates:refresh');
    await new Promise((r) => setTimeout(r, 0));

    expect(p.rows()).toHaveLength(3); // still expanded
    expect(p.cursorRow()?.textContent).toContain('/a.bin');
  });

  test('Enter on a copy collapses its group and puts the cursor back on it', async () => {
    const members = [
      { uuid: 'm1', path: '/a.bin' },
      { uuid: 'm2', path: '/b.bin' },
    ];
    const p = await mountPanel([group('g1', 10, 2, 10)], members);
    await p.invoke('duplicates:next');
    await p.invoke('duplicates:toggle');
    await new Promise((r) => setTimeout(r, 0));
    await p.invoke('duplicates:next');
    await p.invoke('duplicates:toggle'); // from a member row: collapse

    expect(p.rows()).toHaveLength(1);
    expect(p.cursorRow()?.textContent).toContain('g1-hash');
  });

  test('the cursor stops at the top instead of wrapping', async () => {
    const p = await mountPanel([group('g1', 10, 2, 10), group('g2', 8, 2, 8)], []);
    await p.invoke('duplicates:prev');
    expect(p.rows()[0].classList.contains('cursor')).toBe(true);
  });

  test('a copy whose path did not resolve is not trashed', async () => {
    // The record is in the group but its `mfr_path` resolved to nothing: there
    // is no file to move, so the action must say so rather than guess a path.
    const members = [
      { uuid: 'm1', path: '' },
      { uuid: 'm2', path: '/b.bin' },
      { uuid: 'm3', path: '/c.bin' },
    ];
    const p = await mountPanel([group('g1', 10, 3, 20)], members);
    await p.invoke('duplicates:next');
    await p.invoke('duplicates:toggle');
    await new Promise((r) => setTimeout(r, 0));
    expect(p.rows()[1].textContent).toContain('(no path)');
    await p.invoke('duplicates:next');
    await p.invoke('duplicates:trash');

    expect(p.trashPath).not.toHaveBeenCalled();
    expect(p.statusBar.error).toHaveBeenCalled();
  });

  test('a hard-linked copy is marked, and losing that name frees nothing', async () => {
    const members = [
      { uuid: 'm1', path: '/orig', inode: '7' },
      { uuid: 'm2', path: '/link', inode: '7' },
      { uuid: 'm3', path: '/copy' },
    ];
    const p = await mountPanel([group('g1', 16, 3, 16)], members);
    await p.invoke('duplicates:next');
    await p.invoke('duplicates:toggle');
    await new Promise((r) => setTimeout(r, 0));
    expect(p.rows()[1].textContent).toContain('+');

    await p.invoke('duplicates:next'); // /orig
    await p.invoke('duplicates:next'); // /link, the other name of that inode
    await p.invoke('duplicates:trash');

    expect(p.trashPath.mock.calls.map((c) => c[1])).toEqual(['/repo/link']);
    expect(p.groupRows()[0]).toContain(' 2 '); // one name less
    expect(p.groupRows()[0]).toContain('16B'); // and not one byte more to gain
  });

  test('a failing trash stops the run and re-counts what did leave', async () => {
    const members = [
      { uuid: 'm1', path: '/keep.bin' },
      { uuid: 'm2', path: '/copy1.bin' },
      { uuid: 'm3', path: '/copy2.bin' },
      { uuid: 'm4', path: '/copy3.bin' },
    ];
    const p = await mountPanel([group('g1', 10, 4, 30)], members, {
      trashFails: ['/repo/copy2.bin'],
    });
    await p.invoke('duplicates:next');
    await p.invoke('duplicates:toggle');
    await new Promise((r) => setTimeout(r, 0));
    await p.invoke('duplicates:next'); // /keep.bin
    await p.invoke('duplicates:keep');

    expect(p.statusBar.error).toHaveBeenCalled();
    // The first copy went, the failure stopped the rest: three members left.
    expect(p.groupRows()[0]).toContain(' 3 ');
    expect(p.groupRows()[0]).toContain('20B');
  });
});

describe('reclaimableOf', () => {
  test('every member its own inode: all but one copy is recoverable', async () => {
    const mod = await import('../../default-config/panel-types/duplicates/main.js');
    expect(mod.reclaimableOf(10, [{}, {}, {}])).toBe(20);
  });

  test('names sharing an inode count once — removing one frees nothing', async () => {
    const mod = await import('../../default-config/panel-types/duplicates/main.js');
    // Two names on inode 7 plus one separate file: one copy's worth.
    expect(mod.reclaimableOf(16, [{ inode: '7' }, { inode: '7' }, {}])).toBe(16);
  });

  test('a lone member reclaims nothing', async () => {
    const mod = await import('../../default-config/panel-types/duplicates/main.js');
    expect(mod.reclaimableOf(10, [{}])).toBe(0);
  });
});
