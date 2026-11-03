// Exact paths in the panels (doc "Tree names"): a path is a string while it
// is text, `{text, bytes}` when it is not. The shim's helpers do on bytes what
// a panel does on strings (panel-shim/ui.js, panel-shim/file-actions.js), and
// a result that is text is a string again.

import { describe, expect, test } from 'vitest';
import {
  datasetPath,
  el,
  escapeBytes,
  pathBytes,
  pathKey,
  pathOfBytes,
  pathQuery,
  samePath,
} from '../../panel-shim/ui.js';
import { baseName, isWithin, joinPath, parentDir } from '../../panel-shim/file-actions.js';

/** `/d/caf\xE9.txt`, as the backend hands it over. */
const RAW = { text: '/d/caf%E9.txt', bytes: '2f642f636166e92e747874' };

describe('escapeBytes', () => {
  // The daemon's rendering (`TreeName::display`): every byte of an invalid
  // sequence escaped, valid text — "%" included — as it is.
  test.each([
    [[0x63, 0x61, 0x66, 0xe9], 'caf%E9'],
    [[0x31, 0x30, 0x30, 0x25], '100%'],
    [[0xc3, 0xa9, 0xff], 'é%FF'],
    [[0xe2, 0x82], '%E2%82'], // cut short
    [[0xed, 0xa0, 0x80], '%ED%A0%80'], // a surrogate
    [[0xc0, 0xaf], '%C0%AF'], // overlong
    [[0xef, 0xbb, 0xbf, 0x61], '﻿a'], // a BOM is text
  ])('%j shows as %s', (bytes, shown) => {
    expect(escapeBytes(new Uint8Array(bytes))).toBe(shown);
  });
});

describe('the wire form', () => {
  test('text is a string, anything else an object', () => {
    expect(pathOfBytes(new TextEncoder().encode('/a/vidéo'))).toBe('/a/vidéo');
    expect(pathOfBytes(pathBytes(RAW))).toEqual(RAW);
  });

  test('a key tells two paths that show alike apart', () => {
    expect(pathKey('/d/caf%E9.txt')).not.toBe(pathKey(RAW));
    expect(samePath(RAW, { ...RAW })).toBe(true);
    expect(samePath('/a', '/a')).toBe(true);
  });

  test('a URL names text by its text and bytes by their hex', () => {
    expect(pathQuery('/a b')).toBe('path=%2Fa%20b');
    expect(pathQuery(RAW)).toBe(`path_bytes=${RAW.bytes}`);
  });
});

describe('path operations', () => {
  test('a parent that is text is a string again', () => {
    expect(parentDir(RAW)).toBe('/d');
    expect(baseName(RAW)).toEqual({ text: 'caf%E9.txt', bytes: '636166e92e747874' });
    expect(parentDir('/d/x')).toBe('/d');
  });

  test('joining keeps the bytes', () => {
    const dir = { text: '/caf%E9', bytes: '2f636166e9' };
    expect(joinPath(dir, 'x.txt')).toEqual({ text: '/caf%E9/x.txt', bytes: '2f636166e92f782e747874' });
    expect(joinPath('/', baseName(RAW))).toEqual({ text: '/caf%E9.txt', bytes: '2f636166e92e747874' });
    expect(joinPath('/d', 'x')).toBe('/d/x');
  });

  test('within goes by bytes, not by the text', () => {
    const dir = { text: '/caf%E9', bytes: '2f636166e9' };
    expect(isWithin(joinPath(dir, 'x'), dir)).toBe(true);
    expect(isWithin('/caf%E9/x', dir)).toBe(false);
    expect(isWithin(joinPath(dir, 'x'), '/caf%E9')).toBe(false);
    expect(isWithin('/a/b', '/a')).toBe(true);
  });
});

describe('el', () => {
  test('a path that is not text shows its text and keeps its bytes', () => {
    const row = el('li', { 'data-mf-path': RAW }, RAW);
    expect(row.textContent).toBe(RAW.text);
    expect(datasetPath(row.dataset, 'mfPath')).toEqual(RAW);
    expect(datasetPath(el('li', { 'data-mf-path': '/a' }).dataset, 'mfPath')).toBe('/a');
  });
});

describe('the panels act on the exact path', () => {
  test('a shell line names a path that is not text by its bytes', async () => {
    const { shellQuote } = await import('../src/lib/placeholders');
    expect(shellQuote("/a/it's")).toBe("'/a/it'\\''s'");
    expect(shellQuote({ text: "/a/caf%E9'", bytes: '2f612f636166e927' })).toBe(
      "$'/a/caf\\xe9\\x27'",
    );
  });

  test('a tracked child that is not text is found at its bytes', async () => {
    const { loadTrackedChildren } = await import(
      '../../default-config/panel-types/file-manager/tracked.js'
    );
    const daemon = {
      call: async () => [
        { uuid: 'raw', name: { text: 'caf%E9', bytes: '636166e9' } },
        { uuid: 'lit', name: 'caf%E9' },
      ],
    };
    const map = await loadTrackedChildren(daemon, 'r1', '/r', '/r/d', 'dddd');
    expect(map.get({ text: '/r/d/caf%E9', bytes: '2f722f642f636166e9' })).toBe('raw');
    expect(map.get('/r/d/caf%E9')).toBe('lit');
  });

  test('a checked path that is not text is one of its own', async () => {
    const { createPathSelect } = await import('../../panel-shim/multi-select.js');
    const written: unknown[] = [];
    const workspace = {
      get: async () => null,
      set: async (_k: string, v: unknown) => void written.push(v),
      onChange: () => {},
    };
    const checked = createPathSelect({ workspace: workspace as never, fs: { exists: async () => true } });
    await checked.toggle(RAW);
    expect(checked.has({ ...RAW })).toBe(true);
    expect(checked.has(RAW.text)).toBe(false);
    await checked.toggle({ ...RAW });
    expect(checked.count()).toBe(0);
    expect(written).toEqual([[RAW], []]);
  });

  test('a file that is not text is renamed in its own folder', async () => {
    const { runFileCommand } = await import('../src/lib/fileCommands');
    const moves: unknown[][] = [];
    const ops = {
      move: async (from: unknown, to: unknown) => void moves.push([from, to]),
      ask: async () => 'new.txt',
      status: async () => {},
      changed: async () => {},
    };
    const dir = { text: '/caf%E9', bytes: '2f636166e9' };
    const file = joinPath(dir, 'x.txt');
    await runFileCommand('file:rename', { paths: () => [file] }, [], ops as never);
    expect(moves).toEqual([[file, joinPath(dir, 'new.txt')]]);
  });
});
