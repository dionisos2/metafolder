// Shared file-action operations (panel-shim/file-actions.js, served as
// /__file-actions.js): the cut/copy/paste/duplicate/trash menu items any panel
// offers on a file-backed metarecord. Drives the menu-item actions against a
// stub `metafolder`, asserting the fs/trash calls and that errors are reported
// with the error-status timeout (statusErrorMs, or the 8000 fallback).

import { afterEach, describe, expect, test, vi } from 'vitest';
import {
  fileActionsProvider,
  fileMenuItems,
  getClipboard,
  metarecordMenuItems,
  revealFolder,
  rowActionsProvider,
  setClipboard,
} from '/__file-actions.js';

afterEach(() => {
  setClipboard(null);
  vi.unstubAllGlobals();
});

type MenuItem = { label: string; action?: () => void; disabled?: boolean };

function mockMf(
  settings: Record<string, number> | undefined = { statusMessageMs: 1000, statusErrorMs: 2000 },
  fsOverrides: Record<string, unknown> = {},
) {
  const statusBar = { message: vi.fn(async () => {}), error: vi.fn(async () => {}) };
  const fs = {
    // Only the source directory holds "a.txt"; other dirs are empty, so a paste
    // into a fresh target does not dedupe, while a same-dir duplicate does.
    readDir: vi.fn(async (path: string) =>
      path === '/dir' ? [{ name: 'a.txt', path: '/dir/a.txt', is_dir: false }] : [],
    ),
    move: vi.fn(async () => {}),
    copy: vi.fn(async () => {}),
    ...fsOverrides,
  };
  const trash = { trashPath: vi.fn(async () => {}) };
  const workspace = { set: vi.fn(async () => {}) };
  const commands = { invoke: vi.fn(async (_invocation?: string) => {}) };
  const metafolder = {
    statusBar,
    fs,
    trash,
    workspace,
    commands,
    settings,
  } as unknown as MetafolderApi;
  return { metafolder, statusBar, fs, trash, workspace, commands };
}

/** The labels of the entries in a menu (skipping '-' separators and headers). */
function labels(items: unknown[]): string[] {
  return items
    .filter((i): i is MenuItem => i !== '-' && typeof i === 'object' && 'label' in (i as object))
    .map((i) => i.label);
}

/** The menu item with `label` (menu entries are items or '-' separators). */
function item(items: unknown[], label: string): MenuItem {
  const found = items.find((i) => i !== '-' && (i as MenuItem).label === label);
  if (!found) throw new Error(`no menu item "${label}"`);
  return found as MenuItem;
}

describe('file-actions shared operations', () => {
  test('Copy fills the clipboard; Paste copies each entry into a directory target', async () => {
    const { metafolder, fs } = mockMf();
    const onChanged = vi.fn();
    const src = fileMenuItems({ metafolder, repo: 'r', path: '/dir/a.txt', isDir: false, onChanged });
    item(src, 'Copy').action!();
    expect(getClipboard()).toEqual({ paths: ['/dir/a.txt'], mode: 'copy' });

    const dst = fileMenuItems({ metafolder, repo: 'r', path: '/dst', isDir: true, onChanged });
    item(dst, 'Paste').action!();
    await vi.waitFor(() => expect(fs.copy).toHaveBeenCalledWith('/dir/a.txt', '/dst/a.txt'));
    await vi.waitFor(() => expect(onChanged).toHaveBeenCalled());
    setClipboard(null);
  });

  test('Cut then Paste moves and clears the clipboard', async () => {
    const { metafolder, fs } = mockMf();
    const src = fileMenuItems({ metafolder, repo: 'r', path: '/dir/a.txt', isDir: false });
    item(src, 'Cut').action!();
    expect(getClipboard()).toEqual({ paths: ['/dir/a.txt'], mode: 'cut' });

    const dst = fileMenuItems({ metafolder, repo: 'r', path: '/dst', isDir: true });
    item(dst, 'Paste').action!();
    await vi.waitFor(() => expect(fs.move).toHaveBeenCalledWith('/dir/a.txt', '/dst/a.txt'));
    await vi.waitFor(() => expect(getClipboard()).toBeNull());
  });

  test('Paste into itself is refused via statusBar.error with the error timeout', async () => {
    const { metafolder, statusBar, fs } = mockMf();
    setClipboard({ paths: ['/dst'], mode: 'copy' });
    const dst = fileMenuItems({ metafolder, repo: 'r', path: '/dst', isDir: true });
    item(dst, 'Paste').action!();
    await vi.waitFor(() =>
      expect(statusBar.error).toHaveBeenCalledWith(expect.stringContaining('into itself'), 2000),
    );
    expect(fs.copy).not.toHaveBeenCalled();
    setClipboard(null);
  });

  test('a readDir failure during paste surfaces via the error timeout', async () => {
    const { metafolder, statusBar } = mockMf({ statusMessageMs: 1000, statusErrorMs: 2000 }, {
      readDir: vi.fn(async () => {
        throw new Error('denied');
      }),
    });
    setClipboard({ paths: ['/x/y.txt'], mode: 'copy' });
    const dst = fileMenuItems({ metafolder, repo: 'r', path: '/dst', isDir: true });
    item(dst, 'Paste').action!();
    await vi.waitFor(() => expect(statusBar.error).toHaveBeenCalledWith(expect.any(Error), 2000));
    setClipboard(null);
  });

  test('Duplicate copies to a collision-free name in the same directory', async () => {
    const { metafolder, fs } = mockMf();
    const menu = fileMenuItems({ metafolder, repo: 'r', path: '/dir/a.txt', isDir: false });
    item(menu, 'Duplicate').action!();
    // "a.txt" already exists in the stub listing, so it dedupes to "a copy.txt".
    await vi.waitFor(() => expect(fs.copy).toHaveBeenCalledWith('/dir/a.txt', '/dir/a copy.txt'));
  });

  test('errorMs falls back to 8000 when statusErrorMs is unset', async () => {
    const { metafolder, statusBar } = mockMf({ statusMessageMs: 1000 }, {
      readDir: vi.fn(async () => {
        throw new Error('denied');
      }),
    });
    setClipboard({ paths: ['/x/y.txt'], mode: 'copy' });
    const dst = fileMenuItems({ metafolder, repo: 'r', path: '/dst', isDir: true });
    item(dst, 'Paste').action!();
    await vi.waitFor(() => expect(statusBar.error).toHaveBeenCalledWith(expect.any(Error), 8000));
  });

  test('Paste with an empty clipboard reports it and does nothing', async () => {
    const { metafolder, statusBar, fs } = mockMf();
    setClipboard(null);
    const dst = fileMenuItems({ metafolder, repo: 'r', path: '/dst', isDir: true });
    item(dst, 'Paste').action!();
    await vi.waitFor(() =>
      expect(statusBar.message).toHaveBeenCalledWith('clipboard is empty', expect.any(Number)),
    );
    expect(fs.copy).not.toHaveBeenCalled();
  });

  test('Paste of two entries reports the plural count', async () => {
    const { metafolder, statusBar, fs } = mockMf();
    setClipboard({ paths: ['/a/one.txt', '/a/two.txt'], mode: 'copy' });
    const dst = fileMenuItems({ metafolder, repo: 'r', path: '/dst', isDir: true });
    item(dst, 'Paste').action!();
    await vi.waitFor(() => expect(fs.copy).toHaveBeenCalledTimes(2));
    expect(statusBar.message).toHaveBeenCalledWith('Pasted 2 items', expect.any(Number));
  });

  test('Paste onto a file target lands in the file’s parent directory', async () => {
    const { metafolder, fs } = mockMf();
    setClipboard({ paths: ['/a/one.txt'], mode: 'copy' });
    // isDir=false → the paste target is the parent of the clicked file.
    const dst = fileMenuItems({ metafolder, repo: 'r', path: '/dst/here.txt', isDir: false });
    item(dst, 'Paste').action!();
    await vi.waitFor(() => expect(fs.copy).toHaveBeenCalledWith('/a/one.txt', '/dst/one.txt'));
  });

  test('Rename prompts and moves to the new name; a blank answer is a no-op', async () => {
    const { metafolder, fs, statusBar } = mockMf();
    vi.stubGlobal('prompt', vi.fn(() => 'renamed.txt'));
    const menu = fileMenuItems({ metafolder, repo: 'r', path: '/dir/a.txt', isDir: false });
    item(menu, 'Rename…').action!();
    await vi.waitFor(() => expect(fs.move).toHaveBeenCalledWith('/dir/a.txt', '/dir/renamed.txt'));

    vi.stubGlobal('prompt', vi.fn(() => ''));
    fs.move.mockClear();
    item(menu, 'Rename…').action!();
    await Promise.resolve();
    expect(fs.move).not.toHaveBeenCalled();
    expect(statusBar).toBeDefined();
  });

  test('Move to trash confirms then trashes; declining does nothing', async () => {
    const { metafolder, trash } = mockMf();
    vi.stubGlobal('confirm', vi.fn(() => true));
    const menu = fileMenuItems({ metafolder, repo: 'r', path: '/dir/a.txt', name: 'a.txt', isDir: false });
    item(menu, 'Move to trash').action!();
    await vi.waitFor(() => expect(trash.trashPath).toHaveBeenCalledWith('r', '/dir/a.txt'));

    vi.stubGlobal('confirm', vi.fn(() => false));
    (trash.trashPath as ReturnType<typeof vi.fn>).mockClear();
    item(menu, 'Move to trash').action!();
    await Promise.resolve();
    expect(trash.trashPath).not.toHaveBeenCalled();
  });

  test('the default menu falls back to metarecords:dirty when no onChanged is given', async () => {
    const { metafolder, fs, workspace } = mockMf();
    setClipboard({ paths: ['/a/one.txt'], mode: 'copy' });
    const dst = fileMenuItems({ metafolder, repo: 'r', path: '/dst', isDir: true });
    item(dst, 'Paste').action!();
    await vi.waitFor(() => expect(fs.copy).toHaveBeenCalled());
    await vi.waitFor(() =>
      expect(workspace.set).toHaveBeenCalledWith('metarecords:dirty', expect.any(Number)),
    );
  });
});

describe('fileActionsProvider', () => {
  /** A composed-path event whose first tagged node carries `dataset`. */
  function event(dataset: Record<string, string> | null) {
    const node = dataset ? { dataset } : {};
    return { composedPath: () => [node, {}] } as unknown as MouseEvent;
  }

  test('returns no items when there is no active repo', () => {
    const { metafolder } = mockMf();
    const provider = fileActionsProvider(metafolder, () => null);
    expect(provider(event({ mfPath: '/dir/a.txt' }))).toEqual([]);
  });

  test('returns no items when the click was not on a file row', () => {
    const { metafolder } = mockMf();
    const provider = fileActionsProvider(metafolder, () => 'r');
    expect(provider(event(null))).toEqual([]);
    expect(provider(event({ mfPath: '' }))).toEqual([]);
  });

  test('builds the file menu for the tagged row, reading isdir "1"/"0"/unset', () => {
    const { metafolder } = mockMf();
    const provider = fileActionsProvider(metafolder, () => 'r');
    for (const mfIsdir of ['1', '0', undefined]) {
      const dataset: Record<string, string> = { mfPath: '/dir/a.txt', mfName: 'a.txt' };
      if (mfIsdir !== undefined) dataset.mfIsdir = mfIsdir;
      const items = provider(event(dataset));
      expect(items.map((i) => (i === '-' ? '-' : (i as { label: string }).label))).toContain('Cut');
    }
  });
});

describe('revealFolder', () => {
  test('a file opens its containing folder with the file highlighted', () => {
    expect(revealFolder('/a/b/c.txt', false)).toEqual({ dir: '/a/b', select: 'c.txt' });
  });

  test('a directory is opened directly, nothing highlighted', () => {
    expect(revealFolder('/a/b', true)).toEqual({ dir: '/a/b', select: null });
  });

  test('a file directly under the root opens the root', () => {
    expect(revealFolder('/top.txt', false)).toEqual({ dir: '/', select: 'top.txt' });
  });

  test('the root directory opens itself', () => {
    expect(revealFolder('/', true)).toEqual({ dir: '/', select: null });
  });

  test('a trailing slash on a directory path is tolerated', () => {
    expect(revealFolder('/a/b/', true)).toEqual({ dir: '/a/b/', select: null });
  });
});

describe('metarecordMenuItems', () => {
  test('a file-backed metarecord offers detail, file, reveal-folder and Copy UUID', () => {
    const { metafolder, commands } = mockMf();
    const items = metarecordMenuItems({ metafolder, uuid: 'abc', hasFile: true });
    expect(labels(items)).toEqual([
      'Open in panel metarecord-detail',
      'Open in panel file',
      'Open folder in file manager',
      'Copy UUID',
    ]);
    item(items, 'Open in panel metarecord-detail').action!();
    expect(commands.invoke).toHaveBeenCalledWith('panel:reveal-other metarecord-detail');
    item(items, 'Open in panel file').action!();
    expect(commands.invoke).toHaveBeenCalledWith('panel:reveal-other file');
    item(items, 'Open folder in file manager').action!();
    expect(commands.invoke).toHaveBeenCalledWith('file-manager:reveal-folder');
  });

  test('a metarecord with no file offers only detail and Copy UUID', () => {
    const { metafolder } = mockMf();
    const items = metarecordMenuItems({ metafolder, uuid: 'abc', hasFile: false });
    expect(labels(items)).toEqual(['Open in panel metarecord-detail', 'Copy UUID']);
  });

  test('revealFolder:false drops the file-manager item (the panel IS the file manager)', () => {
    const { metafolder } = mockMf();
    const items = metarecordMenuItems({ metafolder, uuid: 'abc', hasFile: true, revealFolder: false });
    expect(labels(items)).toEqual([
      'Open in panel metarecord-detail',
      'Open in panel file',
      'Copy UUID',
    ]);
  });

  test('leading items are inserted right after the Metarecord header', () => {
    const { metafolder, commands } = mockMf();
    const items = metarecordMenuItems({
      metafolder,
      uuid: 'abc',
      hasFile: false,
      leading: [{ label: 'Pick this metarecord', action: () => void commands.invoke('pick:confirm') }],
    });
    expect(labels(items)).toEqual([
      'Pick this metarecord',
      'Open in panel metarecord-detail',
      'Copy UUID',
    ]);
  });

  test('Copy UUID copies the uuid to the clipboard', async () => {
    const writeText = vi.fn(async () => {});
    vi.stubGlobal('navigator', { clipboard: { writeText } });
    const { metafolder } = mockMf();
    const items = metarecordMenuItems({ metafolder, uuid: 'the-uuid', hasFile: false });
    item(items, 'Copy UUID').action!();
    await vi.waitFor(() => expect(writeText).toHaveBeenCalledWith('the-uuid'));
  });
});

describe('rowActionsProvider', () => {
  /** A composed-path event whose first tagged node carries `dataset`. */
  function event(dataset: Record<string, string> | null) {
    const node = dataset ? { dataset } : {};
    return { composedPath: () => [node, {}] } as unknown as MouseEvent;
  }

  test('returns no items when there is no active repo', () => {
    const { metafolder } = mockMf();
    const provider = rowActionsProvider(metafolder, () => null);
    expect(provider(event({ mfUuid: 'abc', mfPath: '/dir/a.txt' }))).toEqual([]);
  });

  test('a file-backed row publishes the selection and offers metarecord + file items', () => {
    const { metafolder, workspace } = mockMf();
    const provider = rowActionsProvider(metafolder, () => 'r');
    const items = provider(event({ mfUuid: 'abc', mfPath: '/dir/a.txt', mfIsdir: '0', mfName: 'a.txt' }));
    expect(workspace.set).toHaveBeenCalledWith('selected_metarecord', { uuid: 'abc', repo: 'r' });
    expect(workspace.set).toHaveBeenCalledWith('selected_paths', ['/dir/a.txt']);
    const l = labels(items);
    expect(l).toContain('Open in panel metarecord-detail');
    expect(l).toContain('Open folder in file manager');
    expect(l).toContain('Cut'); // the file section is appended
  });

  test('a row with a uuid but no file offers only metarecord items and empty paths', () => {
    const { metafolder, workspace } = mockMf();
    const provider = rowActionsProvider(metafolder, () => 'r');
    const items = provider(event({ mfUuid: 'abc' }));
    expect(workspace.set).toHaveBeenCalledWith('selected_paths', []);
    expect(labels(items)).toEqual(['Open in panel metarecord-detail', 'Copy UUID']);
  });

  test('a file row with no uuid falls back to the file section only', () => {
    const { metafolder, workspace } = mockMf();
    const provider = rowActionsProvider(metafolder, () => 'r');
    const items = provider(event({ mfPath: '/dir/a.txt', mfIsdir: '0' }));
    expect(workspace.set).not.toHaveBeenCalledWith('selected_metarecord', expect.anything());
    expect(labels(items)).toContain('Cut');
    expect(labels(items)).not.toContain('Open in panel metarecord-detail');
  });

  test('returns no items when the click was not on a tagged row', () => {
    const { metafolder } = mockMf();
    const provider = rowActionsProvider(metafolder, () => 'r');
    expect(provider(event(null))).toEqual([]);
  });
});
