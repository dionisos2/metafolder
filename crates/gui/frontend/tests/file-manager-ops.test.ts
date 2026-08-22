// file-manager filesystem operations, end to end through the panel's mount
// (spec-gui "file-manager panel type"): the create / clipboard / rename /
// duplicate / delete commands are registered, wired to metafolder.fs and the
// trash, and use collision-free destination names. Mounts the real panel
// against its markup with a controllable stub, then drives the registered
// command handlers.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

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

function stub(repo: string | null) {
  const noop = () => {};
  const handlers = new Map<string, Handler>();
  const fs = {
    readDir: vi.fn(async () => DIR_ENTRIES.map((e) => ({ ...e }))),
    stat: vi.fn(async () => ({})),
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
  const api = {
    ready: Promise.resolve(),
    workspaceId: 'ws-1',
    panelType: 'file-manager',
    pageSize: 100,
    settings: { statusMessageMs: 1000, statusErrorMs: 2000 },
    visible: true,
    onVisibility: noop,
    // Run the deferred start immediately so currentDir is populated.
    whenVisible: (fn: () => void) => fn(),
    bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
    daemon: {
      call: async () => ({ results: [], next_cursor: null }),
      repoRoot: async () => '/',
      repoInternalDir: async () => '/.metafolder/internal',
    },
    cache: { sync: vi.fn(async () => {}), subscribe: vi.fn(() => () => {}) },
    workspace: {
      get: async (key: string) => (key === 'active_repo' ? repo : null),
      set: vi.fn(async () => {}),
      onChange: noop,
    },
    commands: {
      register: (name: string, opts: { handler?: Handler }) => {
        if (opts.handler) handlers.set(name, opts.handler);
        return Promise.resolve(null);
      },
      invoke: () => null,
    },
    fs,
    trash,
    statusBar,
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  return { api, handlers, fs, trash, statusBar };
}

/** Moves the cursor onto `song.mp3` (index 3: '.', '..', dir1, song.mp3). */
async function selectSong(handlers: Map<string, Handler>) {
  for (let i = 0; i < 4; i++) await handlers.get('file-manager:next')!();
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

  async function mount(repo: string | null, { start = true } = {}) {
    const s = stub(repo);
    // With start = false the panel never opens a directory (whenVisible never
    // fires), so currentDir stays null — exercising the guards.
    if (!start) s.api.whenVisible = () => {};
    const root = shadowRoot();
    const mod = await import('../../default-config/panel-types/file-manager/main.js');
    await mod.mount(root, s.api as never);
    // The mount's deferred start awaits an async open(); let it settle.
    await new Promise((r) => setTimeout(r, 0));
    return { ...s, root };
  }

  test('new-folder / new-file create under the current directory', async () => {
    const { handlers, fs } = await mount(null);
    promptSpy.mockReturnValueOnce('My Folder');
    await handlers.get('file-manager:new-folder')!();
    expect(fs.mkdir).toHaveBeenCalledWith('/My Folder');

    promptSpy.mockReturnValueOnce('note.txt');
    await handlers.get('file-manager:new-file')!();
    expect(fs.createFile).toHaveBeenCalledWith('/note.txt');
  });

  test('a blank or cancelled name cancels the create', async () => {
    const { handlers, fs } = await mount(null);
    promptSpy.mockReturnValueOnce('   '); // whitespace only
    await handlers.get('file-manager:new-folder')!();
    promptSpy.mockReturnValueOnce(null); // dialog cancelled
    await handlers.get('file-manager:new-file')!();
    expect(fs.mkdir).not.toHaveBeenCalled();
    expect(fs.createFile).not.toHaveBeenCalled();
  });

  test('operations are inert before the panel has opened a directory', async () => {
    const { handlers, fs, trash } = await mount(null, { start: false });
    promptSpy.mockReturnValue('anything');
    for (const cmd of [
      'file-manager:new-folder',
      'file-manager:new-file',
      'file-manager:rename',
      'file-manager:duplicate',
      'file-manager:paste',
      'file-manager:delete',
    ]) {
      await handlers.get(cmd)!();
    }
    expect(fs.mkdir).not.toHaveBeenCalled();
    expect(fs.createFile).not.toHaveBeenCalled();
    expect(fs.move).not.toHaveBeenCalled();
    expect(fs.copy).not.toHaveBeenCalled();
    expect(fs.remove).not.toHaveBeenCalled();
    expect(trash.trashPath).not.toHaveBeenCalled();
  });

  test('rename moves the selected entry to the new name', async () => {
    const { handlers, fs } = await mount(null);
    await selectSong(handlers);
    promptSpy.mockReturnValueOnce('anthem.mp3');
    await handlers.get('file-manager:rename')!();
    expect(fs.move).toHaveBeenCalledWith('/song.mp3', '/anthem.mp3');
  });

  test('duplicate copies to a collision-free " copy" name', async () => {
    const { handlers, fs } = await mount(null);
    await selectSong(handlers);
    await handlers.get('file-manager:duplicate')!();
    expect(fs.copy).toHaveBeenCalledWith('/song.mp3', '/song copy.mp3');
  });

  test('copy then paste copies into the current directory (deduped)', async () => {
    const { handlers, fs } = await mount(null);
    await selectSong(handlers);
    await handlers.get('file-manager:copy')!();
    await handlers.get('file-manager:paste')!();
    // Pasting song.mp3 back into its own directory dedupes to "song copy.mp3".
    expect(fs.copy).toHaveBeenCalledWith('/song.mp3', '/song copy.mp3');
    expect(fs.move).not.toHaveBeenCalled();
  });

  test('cut then paste moves into the current directory', async () => {
    const { handlers, fs } = await mount(null);
    await selectSong(handlers);
    await handlers.get('file-manager:cut')!();
    await handlers.get('file-manager:paste')!();
    expect(fs.move).toHaveBeenCalledWith('/song.mp3', '/song copy.mp3');
  });

  test('paste with an empty clipboard does nothing', async () => {
    const { handlers, fs } = await mount(null);
    await handlers.get('file-manager:paste')!();
    expect(fs.copy).not.toHaveBeenCalled();
    expect(fs.move).not.toHaveBeenCalled();
  });

  test('delete with no repo removes permanently after confirmation', async () => {
    const { handlers, fs, trash } = await mount(null);
    await selectSong(handlers);
    await handlers.get('file-manager:delete')!();
    expect(confirmSpy).toHaveBeenCalled();
    expect(fs.remove).toHaveBeenCalledWith('/song.mp3');
    expect(trash.trashPath).not.toHaveBeenCalled();
  });

  test('delete with an active repo routes to the trash', async () => {
    const { handlers, fs, trash } = await mount('repo-1');
    await selectSong(handlers);
    await handlers.get('file-manager:delete')!();
    expect(trash.trashPath).toHaveBeenCalledWith('repo-1', '/song.mp3');
    expect(fs.remove).not.toHaveBeenCalled();
  });

  test('a declined confirmation aborts the delete (both paths)', async () => {
    confirmSpy.mockReturnValue(false);
    const noRepo = await mount(null);
    await selectSong(noRepo.handlers);
    await noRepo.handlers.get('file-manager:delete')!();
    expect(noRepo.fs.remove).not.toHaveBeenCalled();

    const withRepo = await mount('repo-1');
    await selectSong(withRepo.handlers);
    await withRepo.handlers.get('file-manager:delete')!();
    expect(withRepo.trash.trashPath).not.toHaveBeenCalled();
  });

  test('operations on the synthetic "." / ".." rows are refused', async () => {
    const { handlers, fs, trash, statusBar } = await mount(null);
    // Cursor at index 0 (the "." row) — not a real entry.
    await handlers.get('file-manager:first')!();
    await handlers.get('file-manager:rename')!();
    await handlers.get('file-manager:duplicate')!();
    await handlers.get('file-manager:delete')!();
    await handlers.get('file-manager:copy')!();
    expect(fs.move).not.toHaveBeenCalled();
    expect(fs.copy).not.toHaveBeenCalled();
    expect(trash.trashPath).not.toHaveBeenCalled();
    expect(statusBar.message).toHaveBeenCalled();
  });

  test('rename to the same name (or blank) is a no-op', async () => {
    const { handlers, fs } = await mount(null);
    await selectSong(handlers);
    promptSpy.mockReturnValueOnce('song.mp3');
    await handlers.get('file-manager:rename')!();
    promptSpy.mockReturnValueOnce('');
    await handlers.get('file-manager:rename')!();
    expect(fs.move).not.toHaveBeenCalled();
  });

  test('a filesystem error is surfaced to the status bar', async () => {
    const { handlers, fs, statusBar } = await mount(null);
    fs.mkdir.mockRejectedValueOnce(new Error('permission denied'));
    promptSpy.mockReturnValueOnce('blocked');
    await handlers.get('file-manager:new-folder')!();
    expect(statusBar.error).toHaveBeenCalled();
  });

  test('navigation, toggles and activation are wired', async () => {
    const { handlers, fs } = await mount('repo-1');
    fs.readDir.mockClear();
    await handlers.get('file-manager:goto-root')!();
    await handlers.get('file-manager:refresh')!();
    await handlers.get('file-manager:toggle-hidden')!();
    await handlers.get('file-manager:toggle-root')!();
    await handlers.get('file-manager:last')!();
    await handlers.get('file-manager:prev')!();
    // Activate the dir1 row (index 2) — opens it.
    await handlers.get('file-manager:first')!();
    await handlers.get('file-manager:next')!();
    await handlers.get('file-manager:next')!();
    await handlers.get('file-manager:activate')!();
    await handlers.get('file-manager:parent')!();
    expect(fs.readDir).toHaveBeenCalled();
  });

  test('mounts with a minimal config (default timing / page size)', async () => {
    const s = stub(null);
    (s.api as { settings: unknown }).settings = {};
    (s.api as { pageSize?: number }).pageSize = undefined;
    const mod = await import('../../default-config/panel-types/file-manager/main.js');
    await mod.mount(shadowRoot(), s.api as never);
    await new Promise((r) => setTimeout(r, 0));
    // A create still works, falling back to the built-in message duration.
    promptSpy.mockReturnValueOnce('x');
    await s.handlers.get('file-manager:new-folder')!();
    expect(s.fs.mkdir).toHaveBeenCalledWith('/x');
  });

  test('rename / duplicate / trash errors are surfaced', async () => {
    const { handlers, fs, trash, statusBar } = await mount('repo-1');
    await selectSong(handlers);
    fs.move.mockRejectedValueOnce(new Error('x'));
    promptSpy.mockReturnValueOnce('r.mp3');
    await handlers.get('file-manager:rename')!();
    fs.copy.mockRejectedValueOnce(new Error('x'));
    await handlers.get('file-manager:duplicate')!();
    trash.trashPath.mockRejectedValueOnce(new Error('x'));
    await handlers.get('file-manager:delete')!();
    expect(statusBar.error).toHaveBeenCalledTimes(3);
  });

  test('new-file and permanent-delete errors are surfaced', async () => {
    const { handlers, fs, statusBar } = await mount(null);
    fs.createFile.mockRejectedValueOnce(new Error('x'));
    promptSpy.mockReturnValueOnce('f');
    await handlers.get('file-manager:new-file')!();
    await selectSong(handlers);
    fs.remove.mockRejectedValueOnce(new Error('x'));
    await handlers.get('file-manager:delete')!();
    expect(statusBar.error).toHaveBeenCalledTimes(2);
  });

  test('paste surfaces per-item errors and refuses pasting into itself', async () => {
    const { handlers, fs, statusBar } = await mount(null);
    await selectSong(handlers);
    await handlers.get('file-manager:copy')!();
    fs.copy.mockRejectedValueOnce(new Error('x'));
    await handlers.get('file-manager:paste')!();
    expect(statusBar.error).toHaveBeenCalled();

    // Cut dir1, navigate into it, then paste — refused (into itself).
    await handlers.get('file-manager:first')!();
    await handlers.get('file-manager:next')!();
    await handlers.get('file-manager:next')!(); // dir1
    await handlers.get('file-manager:cut')!();
    await handlers.get('file-manager:activate')!(); // opens /dir1
    await handlers.get('file-manager:paste')!();
    expect(fs.move).not.toHaveBeenCalled();
  });

  test('the row and background context menus build without throwing', async () => {
    for (const repo of ['repo-1', null] as const) {
      const { root } = await mount(repo);
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
