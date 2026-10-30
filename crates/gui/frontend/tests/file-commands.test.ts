// The `file:` commands (doc "File commands"): what they do to the files a
// panel designates (`paths`) or in the folder it shows (`directory`), from any
// panel providing them — the file manager, the list, the viewer.

import { describe, expect, test, vi } from 'vitest';
import { FILE_COMMANDS, runFileCommand, type FileOps } from '../src/lib/fileCommands';
import { appliesHere, neededFunctions } from '../src/lib/panelFunctions';

type Mocked = { [K in keyof FileOps]: ReturnType<typeof vi.fn> };

function ops(over: Partial<Record<keyof FileOps, ReturnType<typeof vi.fn>>> = {}): FileOps & Mocked {
  return {
    readDir: vi.fn(async () => [{ name: 'a.txt' }, { name: 'a copy.txt' }]),
    move: vi.fn(async () => {}),
    copy: vi.fn(async () => {}),
    mkdir: vi.fn(async () => {}),
    createFile: vi.fn(async () => {}),
    trash: vi.fn(async () => false),
    confirm: vi.fn(async () => true),
    record: vi.fn(async () => true),
    ask: vi.fn(async () => 'typed'),
    clipboard: vi.fn(async () => null),
    clearClipboard: vi.fn(async () => {}),
    status: vi.fn(async () => {}),
    changed: vi.fn(async () => {}),
    ...over,
  } as never;
}

describe('file: commands — what they need of a panel', () => {
  test('the ones on files need the paths, the ones in a folder its directory', () => {
    for (const name of ['file:trash', 'file:rename', 'file:record']) {
      expect(neededFunctions(name, [])).toEqual(['paths']);
    }
    expect(neededFunctions('file:new', ['folder'])).toEqual(['directory']);
    expect(neededFunctions('clipboard:paste', [])).toEqual(['pasteTarget']);
    expect(neededFunctions('file:paste', [])).toBeNull();
    expect(neededFunctions('file:duplicate', [])).toBeNull();
    expect(FILE_COMMANDS).toContain('file:trash');
    // A key bound to one applies only where the panel provides it.
    expect(appliesHere('file:new self folder', 'viewer', () => new Set(['paths']))).toBe(false);
    expect(appliesHere('clipboard:paste self', 'viewer', () => new Set(['pasteTarget']))).toBe(true);
  });
});

describe('file: commands — what they do', () => {
  test('trash sends every designated file to the trash, and the panel reads again', async () => {
    const o = ops();
    const refresh = vi.fn();
    await runFileCommand('file:trash', { paths: () => ['/r/a', '/r/b'], refresh }, [], o);
    expect(o.trash.mock.calls).toEqual([['/r/a'], ['/r/b']]);
    expect(o.status).toHaveBeenCalledWith('Trashed 2 files — restore them from the trash panel');
    expect(o.changed).toHaveBeenCalled();
    expect(refresh).toHaveBeenCalled();
  });

  test('a file no repository holds goes to the desktop trash, and the status says so', async () => {
    const o = ops({ trash: vi.fn(async () => true) });
    await runFileCommand('file:trash', { paths: () => ['/tmp/a.txt'] }, [], o);
    expect(o.status).toHaveBeenCalledWith('Trashed a.txt — restore it from the desktop trash');
  });

  test('trash asks first, naming what goes; declined, nothing goes', async () => {
    const o = ops({ confirm: vi.fn(async () => false) });
    await runFileCommand('file:trash', { paths: () => ['/r/a.txt'] }, [], o);
    expect(o.confirm).toHaveBeenCalledWith('Send "a.txt" to the trash?', false);
    expect(o.trash).not.toHaveBeenCalled();
    await runFileCommand('file:trash', { paths: () => ['/r/a', '/r/b'] }, [], o);
    expect(o.confirm).toHaveBeenLastCalledWith('Send 2 files to the trash?', true);
  });

  test('nothing designated is an error', async () => {
    await expect(runFileCommand('file:trash', { paths: () => [] }, [], ops())).rejects.toThrow(
      'no file here',
    );
  });

  test('rename asks the new name, the old one offered, and moves within the folder', async () => {
    const o = ops({ ask: vi.fn(async () => 'b.txt') });
    const reveal = vi.fn();
    await runFileCommand('file:rename', { paths: () => ['/r/d/a.txt'], reveal }, [], o);
    expect(o.ask).toHaveBeenCalledWith('Rename to:', 'a.txt');
    expect(o.move).toHaveBeenCalledWith('/r/d/a.txt', '/r/d/b.txt');
    expect(reveal).toHaveBeenCalledWith('b.txt');
  });

  test('rename takes the name as an argument, and renames one file at a time', async () => {
    const o = ops();
    await runFileCommand('file:rename', { paths: () => ['/r/a.txt'] }, ['new', 'name'], o);
    expect(o.ask).not.toHaveBeenCalled();
    expect(o.move).toHaveBeenCalledWith('/r/a.txt', '/r/new name');
    await expect(
      runFileCommand('file:rename', { paths: () => ['/r/a', '/r/b'] }, [], ops()),
    ).rejects.toThrow('one file at a time');
  });

  test('rename given up, or to the same name, does nothing', async () => {
    const o = ops({ ask: vi.fn(async () => null) });
    await runFileCommand('file:rename', { paths: () => ['/r/a.txt'] }, [], o);
    await runFileCommand('file:rename', { paths: () => ['/r/a.txt'] }, ['a.txt'], o);
    expect(o.move).not.toHaveBeenCalled();
    expect(o.changed).not.toHaveBeenCalled();
  });

  test('new creates a folder or a file in the directory shown', async () => {
    const o = ops({ ask: vi.fn(async () => 'notes') });
    const reveal = vi.fn();
    await runFileCommand('file:new', { directory: () => '/r/d', reveal }, ['folder'], o);
    expect(o.mkdir).toHaveBeenCalledWith('/r/d/notes');
    expect(reveal).toHaveBeenCalledWith('notes');
    await runFileCommand('file:new', { directory: () => '/r/d' }, ['file', 'x.md'], o);
    expect(o.createFile).toHaveBeenCalledWith('/r/d/x.md');
    await expect(
      runFileCommand('file:new', { directory: () => '/r' }, ['link'], o),
    ).rejects.toThrow('folder / file');
  });

  test('a panel showing no folder has nowhere to create or paste', async () => {
    await expect(
      runFileCommand('file:new', { directory: () => null }, ['file'], ops()),
    ).rejects.toThrow('no folder here');
  });

  const clipboard = (mode: 'copy' | 'cut', ...paths: string[]) => ({
    clipboard: vi.fn(async () => ({ paths, mode })),
  });
  /** Listing a file fails: that is how a paste tells it from a folder. */
  const filesAre = (...files: string[]) => ({
    readDir: vi.fn(async (dir: string) => {
      if (files.includes(dir)) throw new Error('not a directory');
      return [{ name: 'a.txt' }, { name: 'a copy.txt' }];
    }),
  });

  test('paste copies the clipboard into the selected folder, or moves it and empties it', async () => {
    const copied = ops(clipboard('copy', '/x/a.txt'));
    await runFileCommand('clipboard:paste', { pasteTarget: () => '/r' }, [], copied);
    expect(copied.copy).toHaveBeenCalledWith('/x/a.txt', '/r/a copy 2.txt');
    expect(copied.clearClipboard).not.toHaveBeenCalled();

    const cut = ops(clipboard('cut', '/x/b.txt'));
    await runFileCommand('clipboard:paste', { pasteTarget: () => '/r' }, [], cut);
    expect(cut.move).toHaveBeenCalledWith('/x/b.txt', '/r/b.txt');
    expect(cut.clearClipboard).toHaveBeenCalled();
  });

  test('paste on a selected file lands in the folder holding it', async () => {
    const o = ops({ ...clipboard('copy', '/x/b.txt'), ...filesAre('/r/a.txt') });
    await runFileCommand('clipboard:paste', { pasteTarget: () => '/r/a.txt' }, [], o);
    expect(o.copy).toHaveBeenCalledWith('/x/b.txt', '/r/b.txt');
  });

  test('a folder copied and pasted on itself lands beside it, under a free name', async () => {
    const o = ops({ ...clipboard('copy', '/r/d'), readDir: vi.fn(async () => [{ name: 'd' }]) });
    await runFileCommand('clipboard:paste', { pasteTarget: () => '/r/d' }, [], o);
    expect(o.copy).toHaveBeenCalledWith('/r/d', '/r/d copy');
  });

  test('paste never puts a folder into itself or below, and an empty clipboard says so', async () => {
    const cut = ops(clipboard('cut', '/r/d'));
    await runFileCommand('clipboard:paste', { pasteTarget: () => '/r/d' }, [], cut);
    expect(cut.move).not.toHaveBeenCalled();
    const below = ops(clipboard('copy', '/r'));
    await runFileCommand('clipboard:paste', { pasteTarget: () => '/r/sub' }, [], below);
    expect(below.copy).not.toHaveBeenCalled();
    await expect(
      runFileCommand('clipboard:paste', { pasteTarget: () => '/r' }, [], ops()),
    ).rejects.toThrow('clipboard is empty');
  });

  test('paste with nothing selected has nowhere to go', async () => {
    await expect(
      runFileCommand('clipboard:paste', { pasteTarget: () => null }, [], ops(clipboard('copy', '/x/a'))),
    ).rejects.toThrow('no folder here');
  });

  test('paste puts the cursor on what it made, only in the folder shown', async () => {
    const reveal = vi.fn();
    const here = ops(clipboard('copy', '/x/a.txt'));
    const fns = { pasteTarget: () => '/r', directory: () => '/r', reveal };
    await runFileCommand('clipboard:paste', fns, [], here);
    expect(reveal).toHaveBeenCalledWith('a copy 2.txt');
    reveal.mockClear();
    await runFileCommand('clipboard:paste', { ...fns, pasteTarget: () => '/r/sub' }, [], here);
    expect(reveal).not.toHaveBeenCalled();
  });

  test('record gives every designated file its metarecord', async () => {
    const o = ops();
    await runFileCommand('file:record', { paths: () => ['/r/a', '/r/b'] }, [], o);
    expect(o.record.mock.calls).toEqual([['/r/a'], ['/r/b']]);
    expect(o.changed).toHaveBeenCalled();
  });

  test('record takes what the panel says is recordable, when it says', async () => {
    // The file manager designates the checked metarecords' files too, which
    // have their metarecord already: only the checked paths are recorded.
    const o = ops();
    const fns = { paths: () => ['/r/tracked', '/r/new'], recordable: () => ['/r/new'] };
    await runFileCommand('file:record', fns, [], o);
    expect(o.record.mock.calls).toEqual([['/r/new']]);
  });
});
