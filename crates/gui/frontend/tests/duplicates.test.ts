// duplicates panel: choosing which copy survives (doc "duplicates panel").
// Selecting a group row publishes the *group* metarecord, so the detail panel
// can show what the row summarises; "keep this one" trashes the other copies of
// the group under the cursor, then reads the groups again: the daemon has
// re-counted them in the trashing's own write (doc "Duplicate groups").

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { sameNodes } from './support/nodes';
import { generic } from './support/cursor';
import { readFileSync } from 'node:fs';
import { resolve as resolvePath } from 'node:path';
import { withRuns } from './support/runs';

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
  /** `metafolder.settings` (config.toml `[panels]`). */
  settings?: Record<string, unknown>;
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
  // The daemon's side of a trashing: the member leaves its group, which is
  // re-counted, and dissolved with a single copy left.
  const trashPath = vi.fn(async (_repo: string, path: string) => {
    if (options.trashFails?.includes(path)) throw new Error(`cannot trash ${path}`);
    const at = members.findIndex((m) => `/repo${m.path}` === path);
    if (at >= 0) members.splice(at, 1);
    const first = groups[0] as { fields: { name: string; value: Metafolder.Value }[] } | undefined;
    if (first && members.length < 2) groups.splice(0, 1);
    else if (first) {
      // In place: `field()` memoizes a record's index of its fields.
      const count = first.fields.find((f) => f.name === 'mfr_duplicate_count');
      if (count) count.value = int(members.length);
    }
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
      settings: options.settings ?? {},
      defaults: {},
      visible: true,
      onVisibility: noop,
      whenVisible: (fn: () => void) => fn(),
      bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
      daemon: {
        query: async () => ({ records: [], nextCursor: null, total: 0 }),
        treePaths: async (_repo: string, _field: string, uuids: string[]) =>
          Object.fromEntries(uuids.map((uuid) => [uuid, [] as string[]])),
        metarecords: async () => new Map(),
        fields: async () => [],
        request: async () => ({ status: 200, body: null }),
        call: daemonStub(calls, groups, members, options.failLoad),
        resolvePath: async () => '',
        resolveTreeRef: async () => '',
        repoRoot: async () => '/repo',
        repoInternalDir: async () => '/repo/.metafolder/internal',
        metarecordPaths: async () => [],
      },
      changes: { sync: async () => {}, subscribe: () => () => {} },
      query: { parse: async () => null, expand: async () => '', grammarSource: async () => '' },
      pick: { start: async () => '' },
      config: {},
      workspace: {
        get: async (key: string) => store.get(key) ?? null,
        set: setVar,
        adoptRepo: async () => {},
        onChange: noop,
      },
      provide: vi.fn(),
      commands: {
        register: async (name: string, spec: { handler: Handler }) => {
          handlers.set(name, spec.handler);
          return null;
        },
        invoke: vi.fn((_line: string): null => null),
      },
      addKeybinding: async () => null,
      fs: { readDir: async () => [], stat: async () => ({}), exists: async () => true, homeDir: async () => '/home/user' },
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
  mod.mount(shadow, withRuns(api) as never);
  await new Promise((r) => setTimeout(r, 0)); // let the deferred start settle
  const invoke = async (name: string) => {
    // A generic command runs on what the panel provided (doc "Generic panel commands").
    if (name.startsWith('cursor:') || name.startsWith('view:')) return void (await generic(api.provide, name));
    const [command, ...args] = name.split(' ');
    const h = handlers.get(command);
    if (!h) throw new Error(`command not registered: ${command}`);
    await h(...args);
  };
  return {
    shadow,
    api,
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
    await p.invoke('cursor:next'); // onto the group row
    await p.invoke('duplicates:toggle-group'); // expand it
    await new Promise((r) => setTimeout(r, 0));
    p.rows()[1].dispatchEvent(new MouseEvent('click', { bubbles: true }));
    await new Promise((r) => setTimeout(r, 0));
    expect(p.setVar).toHaveBeenCalledWith('selected_metarecord', { uuid: 'm1', repo: 'r' });
  });

  // A held arrow key: the rows stay, only the highlight moves (doc "Moving
  // the cursor").
  test('moving the cursor keeps the rows', async () => {
    const p = await mountPanel([group('g1', 10, 3, 20), group('g2', 5, 2, 5)], []);
    const before = p.rows();
    await p.invoke('cursor:next');
    await p.invoke('cursor:next');
    expect(sameNodes(p.rows(), before)).toBe(true);
    expect(p.cursorRow()).toBe(before[1]);
    expect(p.setVar).toHaveBeenLastCalledWith('selected_paths', []);
  });

  test('keep-this trashes the other copies and the group goes with them', async () => {
    const members = [
      { uuid: 'm1', path: '/keep.bin' },
      { uuid: 'm2', path: '/copy1.bin' },
      { uuid: 'm3', path: '/copy2.bin' },
    ];
    const p = await mountPanel([group('g1', 10, 3, 20)], members);
    await p.invoke('cursor:next');
    await p.invoke('duplicates:toggle-group');
    await new Promise((r) => setTimeout(r, 0));
    await p.invoke('cursor:next'); // onto the first member, the one to keep
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
    await p.invoke('cursor:next'); // the cursor sits on the group row
    await p.invoke('duplicates:keep');
    expect(p.trashPath).not.toHaveBeenCalled();
    expect(p.statusBar.message).toHaveBeenCalled();
  });

  test('keep-this touches several files: it asks even with the confirmation switched off', async () => {
    const confirm = vi.fn(() => true);
    vi.stubGlobal('confirm', confirm);
    const members = [
      { uuid: 'm1', path: '/a.bin' },
      { uuid: 'm2', path: '/b.bin' },
    ];
    const p = await mountPanel([group('g1', 10, 2, 10)], members, {
      settings: { confirmTrash: false },
    });
    await p.invoke('cursor:next');
    await p.invoke('duplicates:toggle-group');
    await new Promise((r) => setTimeout(r, 0));
    await p.invoke('cursor:next');
    await p.invoke('duplicates:keep');
    expect(confirm).toHaveBeenCalled();
    expect(p.trashPath).toHaveBeenCalledWith('r', '/repo/b.bin');
  });

  test('a declined confirmation trashes nothing', async () => {
    vi.stubGlobal('confirm', vi.fn(() => false));
    const members = [
      { uuid: 'm1', path: '/a.bin' },
      { uuid: 'm2', path: '/b.bin' },
    ];
    const p = await mountPanel([group('g1', 10, 2, 10)], members);
    await p.invoke('cursor:next');
    await p.invoke('duplicates:toggle-group');
    await new Promise((r) => setTimeout(r, 0));
    await p.invoke('cursor:next');
    await p.invoke('duplicates:keep');
    expect(p.trashPath).not.toHaveBeenCalled();
    expect(p.rows()).toHaveLength(3);
  });
});

// A group still unfolds in place, by its ▸/▾ button; a copy opens with its ↗
// button (doc "Opening a row"). No double click.
describe('duplicates panel, its buttons', () => {
  beforeEach(() => {
    vi.stubGlobal('confirm', vi.fn(() => true));
    document.body.replaceChildren();
  });
  const members = [
    { uuid: 'm1', path: '/a.bin' },
    { uuid: 'm2', path: '/b.bin' },
  ];
  const tick = () => new Promise((r) => setTimeout(r, 0));

  test("a group's twisty unfolds it, and folds it back", async () => {
    const p = await mountPanel([group('g1', 10, 2, 10)], members);
    p.rows()[0].querySelector<HTMLButtonElement>('button.twisty')!.click();
    await tick();
    expect(p.rows()).toHaveLength(3);
    p.rows()[0].querySelector<HTMLButtonElement>('button.twisty')!.click();
    await tick();
    expect(p.rows()).toHaveLength(1);
  });

  test("a copy's button selects it and shows it in the other slot", async () => {
    const p = await mountPanel([group('g1', 10, 2, 10)], members);
    p.rows()[0].querySelector<HTMLButtonElement>('button.twisty')!.click();
    await tick();
    p.rows()[2].querySelector<HTMLButtonElement>('button.mf-open-show')!.click();
    await tick();
    expect(p.cursorRow()).toBe(p.rows()[2]);
    expect(p.setVar).toHaveBeenCalledWith('selected_metarecord', { uuid: 'm2', repo: 'r' });
    expect(p.setVar).toHaveBeenCalledWith('selected_paths', ['/repo/b.bin']);
    expect(p.api.commands.invoke).toHaveBeenCalledWith(['panel:open', 'other', 'viewer']);
  });

  test("a copy's ⓘ shows it in the detail panel of the other slot", async () => {
    const p = await mountPanel([group('g1', 10, 2, 10)], members);
    p.rows()[0].querySelector<HTMLButtonElement>('button.twisty')!.click();
    await tick();
    p.rows()[1].querySelector<HTMLButtonElement>('button.mf-open-detail')!.click();
    await tick();
    expect(p.cursorRow()).toBe(p.rows()[1]);
    expect(p.setVar).toHaveBeenCalledWith('selected_metarecord', { uuid: 'm1', repo: 'r' });
    expect(p.api.commands.invoke).toHaveBeenCalledWith(['panel:open', 'other', 'metarecord-detail']);
  });

  test('a double click on a group row does not unfold it', async () => {
    const p = await mountPanel([group('g1', 10, 2, 10)], members);
    p.rows()[0].click(); // the cursor onto it, as a double click's first click does
    await tick();
    p.rows()[0].dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
    await tick();
    expect(p.rows()).toHaveLength(1);
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
    await p.invoke('cursor:next');
    await p.invoke('duplicates:toggle-group');
    await new Promise((r) => setTimeout(r, 0));
    await p.invoke('cursor:next'); // onto /a.bin
    await p.invoke('view:refresh');
    await new Promise((r) => setTimeout(r, 0));

    expect(p.rows()).toHaveLength(3); // still expanded
    expect(p.cursorRow()?.textContent).toContain('/a.bin');
  });

  test('moving the cursor publishes the paths too, so a viewer panel follows it', async () => {
    const members = [
      { uuid: 'm1', path: '/a.bin' },
      { uuid: 'm2', path: '/b.bin' },
    ];
    const p = await mountPanel([group('g1', 10, 2, 10)], members);
    await p.invoke('cursor:next');
    expect(p.setVar).toHaveBeenLastCalledWith('selected_paths', []); // a group is no file
    await p.invoke('duplicates:toggle-group');
    await new Promise((r) => setTimeout(r, 0));
    await p.invoke('cursor:next');
    expect(p.setVar).toHaveBeenLastCalledWith('selected_paths', ['/repo/a.bin']);
  });

  test('Enter on a group unfolds it; on a copy shows it in the viewer panel at the target', async () => {
    const members = [
      { uuid: 'm1', path: '/a.bin' },
      { uuid: 'm2', path: '/b.bin' },
    ];
    const p = await mountPanel([group('g1', 10, 2, 10)], members);
    await p.invoke('cursor:next');
    await p.invoke('duplicates:activate other');
    await new Promise((r) => setTimeout(r, 0));
    expect(p.rows()).toHaveLength(3);
    expect(p.api.commands.invoke).not.toHaveBeenCalled();
    await p.invoke('cursor:next');
    await p.invoke('duplicates:activate new');
    expect(p.rows()).toHaveLength(3); // still unfolded
    expect(p.api.commands.invoke).toHaveBeenCalledWith(['panel:open', 'new', 'viewer']);
  });

  test('toggle on a copy collapses its group and puts the cursor back on it', async () => {
    const members = [
      { uuid: 'm1', path: '/a.bin' },
      { uuid: 'm2', path: '/b.bin' },
    ];
    const p = await mountPanel([group('g1', 10, 2, 10)], members);
    await p.invoke('cursor:next');
    await p.invoke('duplicates:toggle-group');
    await new Promise((r) => setTimeout(r, 0));
    await p.invoke('cursor:next');
    await p.invoke('duplicates:toggle-group'); // from a member row: collapse

    expect(p.rows()).toHaveLength(1);
    expect(p.cursorRow()?.textContent).toContain('g1-hash');
  });

  test('the cursor stops at the top instead of wrapping', async () => {
    const p = await mountPanel([group('g1', 10, 2, 10), group('g2', 8, 2, 8)], []);
    await p.invoke('cursor:prev');
    expect(p.rows()[0].classList.contains('cursor')).toBe(true);
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
    await p.invoke('cursor:next');
    await p.invoke('duplicates:toggle-group');
    await new Promise((r) => setTimeout(r, 0));
    await p.invoke('cursor:next'); // /keep.bin
    await p.invoke('duplicates:keep');

    expect(p.statusBar.error).toHaveBeenCalled();
    // The first copy went, the failure stopped the rest: three members left.
    expect(p.trashPath.mock.calls.map((c) => c[1])).toEqual(['/repo/copy1.bin', '/repo/copy2.bin']);
    expect(p.groupRows()[0]).toContain(' 3 ');
  });

  test('keep-this tells the other panels, and the group read again stays open', async () => {
    const members = [
      { uuid: 'm1', path: '/keep.bin' },
      { uuid: 'm2', path: '/copy1.bin' },
      { uuid: 'm3', path: '/copy2.bin' },
      { uuid: 'm4', path: '/copy3.bin' },
    ];
    const p = await mountPanel([group('g1', 10, 4, 30)], members, {
      trashFails: ['/repo/copy3.bin'],
    });
    await p.invoke('cursor:next');
    await p.invoke('duplicates:toggle-group');
    await new Promise((r) => setTimeout(r, 0));
    await p.invoke('cursor:next'); // /keep.bin
    await p.invoke('duplicates:keep');

    expect(p.setVar).toHaveBeenCalledWith('metarecords:dirty', expect.any(Number));
    expect(p.groupRows()[0]).toContain(' 2 ');
    // Still unfolded, the cursor still on the survivor.
    expect(p.rows().filter((li) => li.classList.contains('member'))).toHaveLength(2);
    expect(p.cursorRow()?.textContent).toContain('/keep.bin');
  });

  test('a hard-linked copy is marked', async () => {
    const members = [
      { uuid: 'm1', path: '/orig', inode: '7' },
      { uuid: 'm2', path: '/link', inode: '7' },
      { uuid: 'm3', path: '/copy' },
    ];
    const p = await mountPanel([group('g1', 16, 3, 16)], members);
    await p.invoke('cursor:next');
    await p.invoke('duplicates:toggle-group');
    await new Promise((r) => setTimeout(r, 0));
    expect(p.rows()[1].textContent).toContain('+');
    expect(p.rows()[3].textContent).not.toContain('+');
  });

  test('the panel has no trash command of its own: Delete is item:delete', async () => {
    const p = await mountPanel([group('g1', 10, 2, 10)], []);
    await expect(p.invoke('duplicates:trash')).rejects.toThrow('not registered');
  });
});
