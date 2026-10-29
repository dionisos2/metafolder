// file-manager filesystem operations, end to end through the panel's mount
// (doc "file-manager panel"): the create / clipboard / rename /
// duplicate / delete commands are registered, wired to metafolder.fs and the
// trash, and use collision-free destination names. Mounts the real panel
// against its markup with a controllable stub, then drives the registered
// command handlers.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { cursor, generic } from './support/cursor';

type ProvideMock = Parameters<typeof cursor>[0];
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { withRuns } from './support/runs';
import { globalStub } from './support/global';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/file-manager');

/** The shell's mount path: the panel's body markup into a Shadow root. */
function shadowRoot(): ShadowRoot {
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

// Directory the stub `readDir` returns for '/', re-read after every mutation.
const DIR_ENTRIES = [
  { name: 'dir1', path: '/dir1', is_dir: true },
  { name: 'song.mp3', path: '/song.mp3', is_dir: false },
];

function stub(repo: string | null, vars: Record<string, unknown> = {}) {
  const noop = () => {};
  const handlers = new Map<string, Handler>();
  const fs = {
    readDir: vi.fn(async () => DIR_ENTRIES.map((e) => ({ ...e }))),
    stat: vi.fn(async () => ({})),
    exists: vi.fn(async () => true),
    homeDir: vi.fn(async () => '/home/user'),
    mkdir: vi.fn(async () => {}),
    createFile: vi.fn(async () => {}),
    move: vi.fn(async () => {}),
    copy: vi.fn(async () => {}),
    remove: vi.fn(async () => {}),
  };
  const trash = {
    list: vi.fn(async () => []),
    restore: vi.fn(async () => ''),
    remove: vi.fn(async () => {}),
    empty: vi.fn(async () => 0),
    trashPath: vi.fn(async () => 'song.mp3'),
  };
  const statusBar = { message: vi.fn(async () => {}), error: vi.fn(async () => {}) };
  const global = globalStub();
  const api = {
    ready: Promise.resolve(),
    workspaceId: 'ws-1',
    panelType: 'file-manager',
    pageSize: 100,
    settings: { statusMessageMs: 1000, statusErrorMs: 2000 },
    defaults: {},
    visible: true,
    onVisibility: noop,
    // Run the deferred start immediately so currentDir is populated.
    whenVisible: (fn: () => void) => fn(),
    bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
    daemon: {
      // A `uuid_in` query (the selection's pruning) finds every uuid it names.
      call: async (_method: string, _path: string, body?: { query?: { uuids?: string[] } }) => ({
        results: body?.query?.uuids ?? [],
        next_cursor: null,
      }),
      repoRoot: async () => '/',
      repoInternalDir: async () => '/.metafolder/internal',
      // Every metarecord's file is /<uuid>, but `no-file`'s, which has none.
      treePaths: vi.fn(async (_repo: string, _field: string, uuids: string[]) =>
        Object.fromEntries(uuids.map((u) => [u, u === 'no-file' ? [] : [`/${u}`]])),
      ),
    },
    changes: { sync: vi.fn(async () => {}), subscribe: vi.fn(() => () => {}) },
    workspace: {
      get: async (key: string) => (key === 'active_repo' ? repo : (vars[key] ?? null)),
      set: vi.fn(async () => {}),
      onChange: noop,
    },
    global,
    provide: vi.fn(),
    commands: {
      register: (name: string, opts: { handler?: Handler }) => {
        if (opts.handler) handlers.set(name, opts.handler);
        return Promise.resolve(null);
      },
      invoke: () => null,
    },
    fs,
    trash,
    recent: { touch: vi.fn(async () => {}), list: vi.fn(async () => []) },
    statusBar,
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  return { api, handlers, fs, trash, statusBar, global };
}

/** Moves the cursor onto `song.mp3` (index 3: '.', '..', dir1, song.mp3). */
async function selectSong(api: { provide: ProvideMock }) {
  for (let i = 0; i < 4; i++) await cursor(api.provide, 'next');
}

describe('file-manager filesystem operations', () => {
  let promptSpy: ReturnType<typeof vi.fn>;
  let confirmSpy: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    promptSpy = vi.fn();
    confirmSpy = vi.fn(() => true);
    vi.stubGlobal('prompt', promptSpy);
    vi.stubGlobal('confirm', confirmSpy);
    // jsdom does not implement scrollIntoView, which select() calls.
    Element.prototype.scrollIntoView = () => {};
  });

  async function mount(
    repo: string | null,
    { start = true, vars = {} }: { start?: boolean; vars?: Record<string, unknown> } = {},
  ) {
    const s = stub(repo, vars);
    // With start = false the panel never opens a directory (whenVisible never
    // fires), so currentDir stays null — exercising the guards.
    if (!start) s.api.whenVisible = () => {};
    const root = shadowRoot();
    const mod = await import('../../default-config/panel-types/file-manager/main.js');
    await mod.mount(root, withRuns(s.api) as never);
    // The mount's deferred start awaits an async open(); let it settle.
    await new Promise((r) => setTimeout(r, 0));
    return { ...s, root };
  }

  test('new-folder / new-file create under the current directory', async () => {
    const { api, handlers, fs } = await mount(null);
    promptSpy.mockReturnValueOnce('My Folder');
    await generic(api.provide, 'file:new', 'folder');
    expect(fs.mkdir).toHaveBeenCalledWith('/My Folder');

    promptSpy.mockReturnValueOnce('note.txt');
    await generic(api.provide, 'file:new', 'file');
    expect(fs.createFile).toHaveBeenCalledWith('/note.txt');
  });

  test('a blank or cancelled name cancels the create', async () => {
    const { api, handlers, fs } = await mount(null);
    promptSpy.mockReturnValueOnce('   '); // whitespace only
    await generic(api.provide, 'file:new', 'folder');
    promptSpy.mockReturnValueOnce(null); // dialog cancelled
    await generic(api.provide, 'file:new', 'file');
    expect(fs.mkdir).not.toHaveBeenCalled();
    expect(fs.createFile).not.toHaveBeenCalled();
  });

  test('operations are inert before the panel has opened a directory', async () => {
    const { api, fs, trash } = await mount(null, { start: false });
    promptSpy.mockReturnValue('anything');
    // A panel that shows no folder yet has none to create or paste in, and no
    // file to rename or copy: the file commands refuse.
    await expect(generic(api.provide, 'file:new', 'folder')).rejects.toThrow('no folder here');
    await expect(generic(api.provide, 'file:paste')).rejects.toThrow('no folder here');
    await expect(generic(api.provide, 'file:rename')).rejects.toThrow('no file here');
    await expect(generic(api.provide, 'file:duplicate')).rejects.toThrow('no file here');
    await expect(generic(api.provide, 'file:trash')).rejects.toThrow('no file here');
    expect(fs.mkdir).not.toHaveBeenCalled();
    expect(fs.move).not.toHaveBeenCalled();
    expect(fs.copy).not.toHaveBeenCalled();
    expect(fs.remove).not.toHaveBeenCalled();
    expect(trash.trashPath).not.toHaveBeenCalled();
  });

  test('rename moves the selected entry to the new name', async () => {
    const { api, handlers, fs } = await mount(null);
    await selectSong(api);
    promptSpy.mockReturnValueOnce('anthem.mp3');
    await generic(api.provide, 'file:rename');
    expect(fs.move).toHaveBeenCalledWith('/song.mp3', '/anthem.mp3');
  });

  test('duplicate copies to a collision-free " copy" name', async () => {
    const { api, handlers, fs } = await mount(null);
    await selectSong(api);
    await generic(api.provide, 'file:duplicate');
    expect(fs.copy).toHaveBeenCalledWith('/song.mp3', '/song copy.mp3');
  });

  test('copy then paste copies into the current directory (deduped)', async () => {
    const { api, handlers, fs } = await mount(null);
    await selectSong(api);
    await generic(api.provide, 'clipboard:copy');
    await generic(api.provide, 'file:paste');
    // Pasting song.mp3 back into its own directory dedupes to "song copy.mp3".
    expect(fs.copy).toHaveBeenCalledWith('/song.mp3', '/song copy.mp3');
    expect(fs.move).not.toHaveBeenCalled();
  });

  test('copy and cut fill the global clipboard, which paste empties after a cut', async () => {
    const { api, handlers, global } = await mount(null);
    await selectSong(api);
    await generic(api.provide, 'clipboard:copy');
    expect(global.current('clipboard')).toEqual({ paths: ['/song.mp3'], mode: 'copy' });
    await generic(api.provide, 'clipboard:cut');
    expect(global.current('clipboard')).toEqual({ paths: ['/song.mp3'], mode: 'cut' });
    await generic(api.provide, 'file:paste');
    expect(global.current('clipboard')).toBeNull();
  });

  test('with checked metarecords, copy takes their files, not the row under the cursor', async () => {
    const { api, global } = await mount('repo-1', {
      vars: { selected_metarecords: ['u-a', 'no-file', 'u-b'] },
    });
    await selectSong(api);
    await generic(api.provide, 'clipboard:copy');
    // In check order; a metarecord without a file has nothing to copy.
    expect(global.current('clipboard')).toEqual({ paths: ['/u-a', '/u-b'], mode: 'copy' });
  });

  test('cut then paste moves into the current directory', async () => {
    const { api, handlers, fs } = await mount(null);
    await selectSong(api);
    await generic(api.provide, 'clipboard:cut');
    await generic(api.provide, 'file:paste');
    expect(fs.move).toHaveBeenCalledWith('/song.mp3', '/song copy.mp3');
  });

  test('paste with an empty clipboard does nothing', async () => {
    const { api, fs } = await mount(null);
    await expect(generic(api.provide, 'file:paste')).rejects.toThrow('clipboard is empty');
    expect(fs.copy).not.toHaveBeenCalled();
    expect(fs.move).not.toHaveBeenCalled();
  });

  test('with no repository a file is never deleted for good: it goes to a trash', async () => {
    // The desktop's, chosen by the trash_path command (doc "Trash").
    const { api, handlers, fs, trash } = await mount(null);
    await selectSong(api);
    await generic(api.provide, 'file:trash');
    expect(trash.trashPath).toHaveBeenCalledWith(null, '/song.mp3');
    expect(confirmSpy).not.toHaveBeenCalled();
    expect(fs.remove).not.toHaveBeenCalled();
    expect(handlers.has('file-manager:delete')).toBe(false);
  });

  test('operations on the synthetic "." / ".." rows are refused', async () => {
    const { api, fs, trash } = await mount(null);
    // Cursor at index 0 (the "." row) — not a real entry.
    await cursor(api.provide, 'first');
    await expect(generic(api.provide, 'file:rename')).rejects.toThrow('no file here');
    await expect(generic(api.provide, 'file:duplicate')).rejects.toThrow('no file here');
    await expect(generic(api.provide, 'file:trash')).rejects.toThrow('no file here');
    await expect(generic(api.provide, 'clipboard:copy')).rejects.toThrow('no file to copy');
    expect(fs.move).not.toHaveBeenCalled();
    expect(fs.copy).not.toHaveBeenCalled();
    expect(trash.trashPath).not.toHaveBeenCalled();
  });

  test('rename to the same name (or blank) is a no-op', async () => {
    const { api, handlers, fs } = await mount(null);
    await selectSong(api);
    promptSpy.mockReturnValueOnce('song.mp3');
    await generic(api.provide, 'file:rename');
    promptSpy.mockReturnValueOnce('');
    await generic(api.provide, 'file:rename');
    expect(fs.move).not.toHaveBeenCalled();
  });

  test('a filesystem error fails the command, which the shell reports', async () => {
    const { api, fs } = await mount(null);
    fs.mkdir.mockRejectedValueOnce(new Error('permission denied'));
    promptSpy.mockReturnValueOnce('blocked');
    await expect(generic(api.provide, 'file:new', 'folder')).rejects.toThrow('permission denied');
  });

  test('navigation, toggles and activation are wired', async () => {
    const { api, handlers, fs } = await mount('repo-1');
    fs.readDir.mockClear();
    await generic(api.provide, 'cursor:root');
    await generic(api.provide, 'view:refresh');
    await handlers.get('file-manager:toggle')!('hidden');
    await handlers.get('file-manager:toggle')!('root');
    await cursor(api.provide, 'last');
    await cursor(api.provide, 'prev');
    // Activate the dir1 row (index 2) — opens it.
    await cursor(api.provide, 'first');
    await cursor(api.provide, 'next');
    await cursor(api.provide, 'next');
    await handlers.get('file-manager:activate')!();
    await generic(api.provide, 'cursor:parent');
    expect(fs.readDir).toHaveBeenCalled();
  });

  // Browsing the disk with no repository, "repo root" names nothing: the button
  // says "root" and goes to the filesystem root, so the jump is never a dead end.
  test('with no active repository the root button jumps to the disk root', async () => {
    const { api, handlers, root } = await mount(null);
    expect(root.getElementById('goto-root')!.textContent).toBe('root');
    // Descend into dir1 (index 2: '.', '..', dir1) so the jump has somewhere
    // to come back from.
    await cursor(api.provide, 'first');
    await cursor(api.provide, 'next');
    await cursor(api.provide, 'next');
    await handlers.get('file-manager:activate')!();
    expect(root.getElementById('current-path')!.textContent).toBe('/dir1');

    await generic(api.provide, 'cursor:root');
    expect(root.getElementById('current-path')!.textContent).toBe('/');
  });

  test('with a repository the root button still names the repo root', async () => {
    const { api, root } = await mount('repo-1');
    expect(root.getElementById('goto-root')!.textContent).toBe('repo root');
  });

  test('mounts with a minimal config (default timing / page size)', async () => {
    const s = stub(null);
    (s.api as { settings: unknown }).settings = {};
    (s.api as { pageSize?: number }).pageSize = undefined;
    const mod = await import('../../default-config/panel-types/file-manager/main.js');
    await mod.mount(shadowRoot(), withRuns(s.api) as never);
    await new Promise((r) => setTimeout(r, 0));
    // A create still works.
    promptSpy.mockReturnValueOnce('x');
    await generic(s.api.provide, 'file:new', 'folder');
    expect(s.fs.mkdir).toHaveBeenCalledWith('/x');
  });

  test('rename / duplicate errors fail the command, a trash error is surfaced', async () => {
    const { api, fs, trash } = await mount('repo-1');
    await selectSong(api);
    fs.move.mockRejectedValueOnce(new Error('x'));
    promptSpy.mockReturnValueOnce('r.mp3');
    await expect(generic(api.provide, 'file:rename')).rejects.toThrow('x');
    fs.copy.mockRejectedValueOnce(new Error('y'));
    await expect(generic(api.provide, 'file:duplicate')).rejects.toThrow('y');
    trash.trashPath.mockRejectedValueOnce(new Error('z'));
    await expect(generic(api.provide, 'file:trash')).rejects.toThrow('z');
  });

  test('new-file errors fail the command', async () => {
    const { api, fs } = await mount(null);
    fs.createFile.mockRejectedValueOnce(new Error('x'));
    promptSpy.mockReturnValueOnce('f');
    await expect(generic(api.provide, 'file:new', 'file')).rejects.toThrow('x');
  });

  test('paste fails on a copy error, and refuses pasting into itself', async () => {
    const { api, handlers, fs } = await mount(null);
    await selectSong(api);
    await generic(api.provide, 'clipboard:copy');
    fs.copy.mockRejectedValueOnce(new Error('x'));
    await expect(generic(api.provide, 'file:paste')).rejects.toThrow('x');

    // Cut dir1, navigate into it, then paste — refused (into itself).
    await cursor(api.provide, 'first');
    await cursor(api.provide, 'next');
    await cursor(api.provide, 'next'); // dir1
    await generic(api.provide, 'clipboard:cut');
    await handlers.get('file-manager:activate')!(); // opens /dir1
    await generic(api.provide, 'file:paste');
    expect(fs.move).not.toHaveBeenCalled();
  });

  test('the "." row of a tracked directory offers the Metarecord section', async () => {
    // The current directory itself is a tracked metarecord (its tree node), so
    // right-clicking the synthetic "." row must offer the same "Metarecord"
    // section a real entry does — even though "." is not a directory entry.
    const s = stub('repo-1');
    const captured: { header?: string; label?: string }[][] = [];
    s.api.daemon.call = (async (_method: string, path: string) => {
      if (path.includes('/tree/resolve-path')) return { uuid: 'dir-uuid' };
      if (path.includes('/tree/children')) return [];
      return { results: [], next_cursor: null };
    }) as never;
    s.api.contextMenu = Object.assign(
      (_e: MouseEvent, items: { header?: string }[]) => captured.push(items),
      { addDefaultItems: () => {} },
    ) as never;
    const root = shadowRoot();
    const mod = await import('../../default-config/panel-types/file-manager/main.js');
    await mod.mount(root, withRuns(s.api) as never);
    await new Promise((r) => setTimeout(r, 0));
    const dotRow = root.querySelectorAll('li')[0]; // the synthetic "." row
    dotRow.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true }));
    const items = captured.at(-1)!;
    expect(items.some((it) => it && it.header === 'Metarecord')).toBe(true);
  });

  test('a row menu normalizes to the canonical category order', async () => {
    // The panel pushes its categories in the order that reads best in the
    // source; what the user sees is orderMenu's canonical one, the same in
    // every panel (doc "Context menus").
    const { orderMenu } = await import('../../panel-shim/menu.js');
    const s = stub('repo-1');
    const captured: Metafolder.MenuItem[][] = [];
    s.api.contextMenu = Object.assign(
      (_e: MouseEvent, items: Metafolder.MenuItem[]) => captured.push(items),
      { addDefaultItems: () => {} },
    ) as never;
    const root = shadowRoot();
    const mod = await import('../../default-config/panel-types/file-manager/main.js');
    await mod.mount(root, withRuns(s.api) as never);
    await new Promise((r) => setTimeout(r, 0));
    const fileRow = [...root.querySelectorAll('li')].at(-1)!;
    fileRow.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true }));
    const headers = orderMenu(captured.at(-1)!)
      .filter((item) => item !== '-' && 'header' in item)
      .map((item) => item.header);
    expect(headers).toEqual(['Metarecord', 'File', 'Directory']);
  });

  test('the row and background context menus build without throwing', async () => {
    for (const repo of ['repo-1', null] as const) {
      const { api, root } = await mount(repo);
      // A directory row, a file row, and the synthetic "." / ".." rows.
      for (const row of root.querySelectorAll('li')) {
        row.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true }));
      }
      const listing = root.getElementById('listing')!;
      listing.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true }));
      expect(root.querySelectorAll('li').length).toBeGreaterThan(0);
    }
  });
});
