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
    trash: vi.fn(async () => {}),
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
    for (const name of ['file:trash', 'file:rename', 'file:duplicate', 'file:record']) {
      expect(neededFunctions(name, [])).toEqual(['paths']);
    }
    expect(neededFunctions('file:new', ['folder'])).toEqual(['directory']);
    expect(neededFunctions('file:paste', [])).toEqual(['directory']);
    expect(FILE_COMMANDS).toContain('file:trash');
    // A key bound to one applies only where the panel provides it.
    expect(appliesHere('file:paste self', 'viewer', () => new Set(['paths']))).toBe(false);
    expect(appliesHere('file:paste self', 'file-manager', () => new Set(['directory']))).toBe(
      true,
    );
  });
});

describe('file: commands — what they do', () => {
  test('trash sends every designated file to the trash, and the panel reads again', async () => {
    const o = ops();
    const refresh = vi.fn();
    await runFileCommand('file:trash', { paths: () => ['/r/a', '/r/b'], refresh }, [], o);
    expect(o.trash.mock.calls).toEqual([['/r/a'], ['/r/b']]);
    expect(o.changed).toHaveBeenCalled();
    expect(refresh).toHaveBeenCalled();
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

  test('duplicate copies beside the original under a free name', async () => {
    const o = ops();
    await runFileCommand('file:duplicate', { paths: () => ['/r/a.txt'] }, [], o);
    expect(o.readDir).toHaveBeenCalledWith('/r');
    expect(o.copy).toHaveBeenCalledWith('/r/a.txt', '/r/a copy 2.txt');
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

  test('paste copies the clipboard into the directory, or moves it and empties it', async () => {
    const copied = ops({ clipboard: vi.fn(async () => ({ paths: ['/x/a.txt'], mode: 'copy' })) });
    await runFileCommand('file:paste', { directory: () => '/r' }, [], copied);
    expect(copied.copy).toHaveBeenCalledWith('/x/a.txt', '/r/a copy 2.txt');
    expect(copied.clearClipboard).not.toHaveBeenCalled();

    const cut = ops({ clipboard: vi.fn(async () => ({ paths: ['/x/b.txt'], mode: 'cut' })) });
    await runFileCommand('file:paste', { directory: () => '/r' }, [], cut);
    expect(cut.move).toHaveBeenCalledWith('/x/b.txt', '/r/b.txt');
    expect(cut.clearClipboard).toHaveBeenCalled();
  });

  test('paste never puts a folder into itself, and an empty clipboard says so', async () => {
    const o = ops({ clipboard: vi.fn(async () => ({ paths: ['/r'], mode: 'cut' })) });
    await runFileCommand('file:paste', { directory: () => '/r/sub' }, [], o);
    expect(o.move).not.toHaveBeenCalled();
    await expect(
      runFileCommand('file:paste', { directory: () => '/r' }, [], ops()),
    ).rejects.toThrow('clipboard is empty');
  });

  test('record gives every designated file its metarecord', async () => {
    const o = ops();
    await runFileCommand('file:record', { paths: () => ['/r/a', '/r/b'] }, [], o);
    expect(o.record.mock.calls).toEqual([['/r/a'], ['/r/b']]);
    expect(o.changed).toHaveBeenCalled();
  });
});
