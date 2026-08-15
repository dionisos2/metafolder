// file-manager filesystem-operation helpers (spec-gui "file-manager panel
// type"): path joining and collision-free destination naming for the
// new-folder/new-file, copy/cut/paste, duplicate and rename actions.

import { describe, expect, test } from 'vitest';
import {
  joinPath,
  splitExt,
  dedupeName,
} from '../../default-config/panel-types/file-manager/fileops.js';

describe('joinPath', () => {
  test('joins a name onto a directory', () => {
    expect(joinPath('/data/repo', 'foo.txt')).toBe('/data/repo/foo.txt');
  });

  test('the filesystem root does not double the slash', () => {
    expect(joinPath('/', 'foo.txt')).toBe('/foo.txt');
  });
});

describe('splitExt', () => {
  test('splits the last extension', () => {
    expect(splitExt('foo.txt')).toEqual(['foo', '.txt']);
    expect(splitExt('archive.tar.gz')).toEqual(['archive.tar', '.gz']);
  });

  test('no extension yields an empty suffix', () => {
    expect(splitExt('README')).toEqual(['README', '']);
  });

  test('a leading dot is a hidden name, not an extension', () => {
    expect(splitExt('.bashrc')).toEqual(['.bashrc', '']);
  });

  test('a trailing dot is kept on the stem', () => {
    expect(splitExt('weird.')).toEqual(['weird.', '']);
  });
});

describe('dedupeName', () => {
  test('a free name is returned unchanged', () => {
    expect(dedupeName('foo.txt', new Set(['bar.txt']))).toBe('foo.txt');
  });

  test('a collision inserts " copy" before the extension', () => {
    expect(dedupeName('foo.txt', new Set(['foo.txt']))).toBe('foo copy.txt');
  });

  test('further collisions number the copy', () => {
    const taken = new Set(['foo.txt', 'foo copy.txt']);
    expect(dedupeName('foo.txt', taken)).toBe('foo copy 2.txt');
    taken.add('foo copy 2.txt');
    expect(dedupeName('foo.txt', taken)).toBe('foo copy 3.txt');
  });

  test('an extension-less name (e.g. a directory) appends " copy"', () => {
    expect(dedupeName('music', new Set(['music']))).toBe('music copy');
    expect(dedupeName('music', new Set(['music', 'music copy']))).toBe('music copy 2');
  });

  test('the copy suffix keeps a multi-part extension intact', () => {
    expect(dedupeName('archive.tar.gz', new Set(['archive.tar.gz']))).toBe('archive.tar copy.gz');
  });
});
