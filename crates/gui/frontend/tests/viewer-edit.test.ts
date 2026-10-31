// Editing a text file in the viewer panel (doc "Editing a text file"): the box,
// the save and reload buttons, their commands, and what must never happen — a
// truncated or re-encoded file written back, a change on disk overwritten
// unseen, unsaved text lost to a move of the selection.

import { afterEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { editableText, encodeEdited } from '../../default-config/panel-types/viewer/textedit.js';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/viewer');
const encode = (text: string) => new TextEncoder().encode(text);

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

const flush = async () => {
  for (let i = 0; i < 8; i += 1) await new Promise((r) => setTimeout(r, 0));
};

type Handler = (mf: unknown, ...args: string[]) => unknown;

/** A viewer mounted on a fake disk of `files`. */
async function mounted(files: Record<string, string | Uint8Array>, selected: string, defaults = {}) {
  const disk = new Map(Object.entries(files));
  const mtimes = new Map<string, number>([...disk.keys()].map((path) => [path, 1000]));
  const bytesOf = (path: string) => {
    const content = disk.get(path) ?? '';
    return typeof content === 'string' ? encode(content) : content;
  };
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: string) => {
      const path = new URL(url).searchParams.get('path') ?? '';
      return {
        ok: true,
        status: 200,
        json: async () => ({}),
        arrayBuffer: async () => bytesOf(path).slice().buffer,
      };
    }),
  );
  const vars = new Map<string, unknown>([
    ['selected_paths', [selected]],
    ['selected_metarecord', null],
    ['active_repo', null],
  ]);
  const listeners = new Map<string, (value: unknown) => void>();
  const handlers = new Map<string, Handler>();
  const provided: { refresh?: (mf?: unknown) => unknown } = {};
  const noop = () => {};
  const api = {
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'test-token',
    pageSize: 10,
    settings: {},
    defaults,
    whenVisible: (fn: () => void) => fn(),
    provide: (fns: object) => Object.assign(provided, fns),
    workspace: {
      get: async (key: string) => vars.get(key) ?? null,
      set: vi.fn(async () => {}),
      onChange: (key: string, fn: (value: unknown) => void) => listeners.set(key, fn),
    },
    commands: {
      register: (name: string, options: { handler: Handler }) => {
        handlers.set(name, options.handler);
        return Promise.resolve(null);
      },
      invoke: () => null,
    },
    daemon: { call: async () => ({}), repoRoot: async () => '/' },
    fs: {
      stat: vi.fn(async (path: string) => {
        if (!disk.has(path)) throw new Error(`cannot stat ${path}`);
        return { is_dir: false, kind: null, mtime: mtimes.get(path), size: bytesOf(path).length };
      }),
      exists: async () => true,
      readDir: async () => [],
      writeText: vi.fn(async (path: string, text: string) => {
        disk.set(path, text);
        mtimes.set(path, (mtimes.get(path) ?? 0) + 1);
      }),
    },
    statusBar: { message: vi.fn(async () => {}), error: vi.fn(async () => {}) },
    recent: { touch: vi.fn(async () => {}) },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  const root = shadowRoot();
  const mod = await import('../../default-config/panel-types/viewer/main.js');
  await mod.mount(root, api as never);
  await flush();
  const run = async (name: string, ...args: string[]) => {
    await handlers.get(name)!(api, ...args);
    await flush();
  };
  const area = () => root.querySelector<HTMLTextAreaElement>('#viewer textarea');
  const type = (text: string) => {
    const field = area()!;
    field.value = text;
    field.dispatchEvent(new Event('input', { bubbles: true }));
  };
  const select = async (path: string) => {
    vars.set('selected_paths', [path]);
    listeners.get('selected_paths')!([path]);
    await flush();
  };
  const changeOnDisk = (path: string, content: string) => {
    disk.set(path, content);
    mtimes.set(path, (mtimes.get(path) ?? 0) + 100);
  };
  return { root, api, provided, run, area, type, select, disk, changeOnDisk };
}

const button = (root: ShadowRoot, id: string) => root.getElementById(id) as HTMLButtonElement;
const box = (root: ShadowRoot) => root.getElementById('text-edit') as HTMLInputElement;

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  document.body.replaceChildren();
});

describe('editableText', () => {
  test('text in UTF-8 is editable, whole', () => {
    expect(editableText(encode('héllo\nworld\n'), false)).toEqual({
      ok: true,
      text: 'héllo\nworld\n',
      eol: '\n',
    });
  });

  test('a truncated file is not: saving it would cut the rest', () => {
    const verdict = editableText(encode('abc'), true);
    expect(verdict.ok).toBe(false);
    expect(verdict.ok === false && verdict.reason).toMatch(/too large/);
  });

  test('bytes that are not UTF-8 are not: saving them would destroy them', () => {
    expect(editableText(new Uint8Array([0x63, 0x61, 0x66, 0xe9]), false).ok).toBe(false);
    // UTF-16, which a listed extension previews.
    expect(editableText(new Uint8Array([0xff, 0xfe, 0x61, 0x00]), false).ok).toBe(false);
  });

  test('a NUL is not text to edit', () => {
    expect(editableText(new Uint8Array([0x61, 0x00, 0x62]), false).ok).toBe(false);
  });

  test('a byte order mark survives the round trip', () => {
    const verdict = editableText(new Uint8Array([0xef, 0xbb, 0xbf, 0x61]), false);
    expect(verdict.ok).toBe(true);
    expect(verdict.ok && verdict.text).toBe('﻿a');
  });

  test('CRLF line ends are given back as they were: a text field only knows LF', () => {
    const verdict = editableText(encode('a\r\nb\r\n'), false);
    expect(verdict).toEqual({ ok: true, text: 'a\nb\n', eol: '\r\n' });
    expect(encodeEdited('a\nb\nc\n', '\r\n')).toBe('a\r\nb\r\nc\r\n');
    expect(encodeEdited('a\nb\n', '\n')).toBe('a\nb\n');
  });

  test('mixed line ends are not editable: a text field would flatten them', () => {
    expect(editableText(encode('a\r\nb\n'), false).ok).toBe(false);
    expect(editableText(encode('a\rb'), false).ok).toBe(false);
  });
});

describe('viewer panel — editing a text file', () => {
  test('the toolbar is there for a text file, not for the rest', async () => {
    const { root, select, api } = await mounted({ '/a.txt': 'one\n', '/b.png': 'x' }, '/a.txt');
    expect(root.getElementById('text-toolbar')!.hidden).toBe(false);
    expect(box(root).checked).toBe(false);
    expect(button(root, 'text-save').disabled).toBe(true);

    api.fs.stat.mockImplementation(
      async () => ({ is_dir: false, kind: 'image', mtime: 1, size: 1 }) as never,
    );
    await select('/b.png');
    expect(root.getElementById('text-toolbar')!.hidden).toBe(true);
  });

  test('the box turns the preview into a field holding the file', async () => {
    const { root, area } = await mounted({ '/a.txt': 'one\n' }, '/a.txt');
    expect(area()).toBeNull();
    box(root).click();
    await flush();
    expect(area()!.value).toBe('one\n');

    box(root).click();
    await flush();
    expect(area()).toBeNull();
    expect(root.querySelector('#viewer pre')!.textContent).toBe('one\n');
  });

  test('save is greyed until something changes, and again once saved', async () => {
    const { root, run, type, api, disk } = await mounted({ '/a.txt': 'one\n' }, '/a.txt');
    await run('viewer:toggle-edit');
    const save = button(root, 'text-save');
    expect(save.disabled).toBe(true);

    type('two\n');
    expect(save.disabled).toBe(false);
    type('one\n');
    expect(save.disabled).toBe(true);

    type('two\n');
    save.click();
    await flush();
    expect(api.fs.writeText).toHaveBeenCalledWith('/a.txt', 'two\n');
    expect(disk.get('/a.txt')).toBe('two\n');
    expect(save.disabled).toBe(true);
  });

  test('viewer:save with nothing changed writes nothing', async () => {
    const { run, api } = await mounted({ '/a.txt': 'one\n' }, '/a.txt');
    await run('viewer:toggle-edit');
    await run('viewer:save');
    expect(api.fs.writeText).not.toHaveBeenCalled();
  });

  test('a CRLF file is saved in CRLF', async () => {
    const { run, type, api } = await mounted({ '/a.txt': 'one\r\n' }, '/a.txt');
    await run('viewer:toggle-edit');
    type('one\ntwo\n');
    await run('viewer:save');
    expect(api.fs.writeText).toHaveBeenCalledWith('/a.txt', 'one\r\ntwo\r\n');
  });

  test('reload drops the changes and reads the file as it is now', async () => {
    const { root, type, run, area, changeOnDisk } = await mounted({ '/a.txt': 'one\n' }, '/a.txt');
    await run('viewer:toggle-edit');
    type('mine\n');
    changeOnDisk('/a.txt', 'theirs\n');
    button(root, 'text-reload').click();
    await flush();
    expect(area()!.value).toBe('theirs\n');
    expect(button(root, 'text-save').disabled).toBe(true);
  });

  test('a file changed on disk is not overwritten unless asked', async () => {
    const { run, type, api, disk, changeOnDisk } = await mounted({ '/a.txt': 'one\n' }, '/a.txt');
    await run('viewer:toggle-edit');
    type('mine\n');
    changeOnDisk('/a.txt', 'theirs\n');

    await run('viewer:save');
    expect(api.fs.writeText).not.toHaveBeenCalled();
    expect(disk.get('/a.txt')).toBe('theirs\n');
    expect(api.statusBar.error).toHaveBeenCalledWith(expect.stringMatching(/changed on disk/));

    await run('viewer:save', 'force');
    expect(disk.get('/a.txt')).toBe('mine\n');
  });

  test('its own save is not taken for a change on disk', async () => {
    const { run, type, api, disk } = await mounted({ '/a.txt': 'one\n' }, '/a.txt');
    await run('viewer:toggle-edit');
    type('two\n');
    await run('viewer:save');
    type('three\n');
    await run('viewer:save');
    expect(api.statusBar.error).not.toHaveBeenCalled();
    expect(disk.get('/a.txt')).toBe('three\n');
  });

  test('a failed write keeps the changes', async () => {
    const { root, run, type, area, api } = await mounted({ '/a.txt': 'one\n' }, '/a.txt');
    await run('viewer:toggle-edit');
    type('mine\n');
    api.fs.writeText.mockRejectedValueOnce(new Error('cannot write /a.txt: read-only'));
    await run('viewer:save');
    expect(api.statusBar.error).toHaveBeenCalledWith(expect.stringMatching(/read-only/));
    expect(area()!.value).toBe('mine\n');
    expect(button(root, 'text-save').disabled).toBe(false);
  });

  test('a file that cannot be written back whole cannot be edited, and says why', async () => {
    const { root, run, area, api } = await mounted(
      { '/big.txt': 'x'.repeat(100) },
      '/big.txt',
      { textPreviewLimit: 10 },
    );
    expect(box(root).disabled).toBe(true);
    expect(root.getElementById('text-note')!.textContent).toMatch(/too large/);
    await run('viewer:toggle-edit');
    expect(area()).toBeNull();
    expect(api.statusBar.message).toHaveBeenCalledWith(
      expect.stringMatching(/too large/),
      expect.anything(),
    );
  });

  test('unsaved changes hold the file when the selection moves', async () => {
    const { root, run, type, select, area } = await mounted(
      { '/a.txt': 'one\n', '/b.txt': 'bee\n' },
      '/a.txt',
    );
    await run('viewer:toggle-edit');
    type('mine\n');
    await select('/b.txt');
    expect(area()!.value).toBe('mine\n');
    expect(root.getElementById('path-bar')!.textContent).toContain('/a.txt');

    // Saved: the panel goes where the selection went meanwhile.
    await run('viewer:save');
    expect(area()!.value).toBe('bee\n');
    expect(root.getElementById('path-bar')!.textContent).toContain('/b.txt');
  });

  test('reload too lets the held selection through', async () => {
    const { run, type, select, area, disk } = await mounted(
      { '/a.txt': 'one\n', '/b.txt': 'bee\n' },
      '/a.txt',
    );
    await run('viewer:toggle-edit');
    type('mine\n');
    await select('/b.txt');
    await run('viewer:reload');
    expect(area()!.value).toBe('bee\n');
    expect(disk.get('/a.txt')).toBe('one\n');
  });

  test('without changes the box stays on and the next file is editable', async () => {
    const { run, select, area } = await mounted({ '/a.txt': 'one\n', '/b.txt': 'bee\n' }, '/a.txt');
    await run('viewer:toggle-edit');
    await select('/b.txt');
    expect(area()!.value).toBe('bee\n');
  });

  test('stepping out of the box keeps the changes, shown and savable', async () => {
    const { root, run, type, disk } = await mounted({ '/a.txt': 'one\n' }, '/a.txt');
    await run('viewer:toggle-edit');
    type('mine\n');
    await run('viewer:toggle-edit');
    expect(root.querySelector('#viewer pre')!.textContent).toBe('mine\n');
    expect(button(root, 'text-save').disabled).toBe(false);
    await run('viewer:save');
    expect(disk.get('/a.txt')).toBe('mine\n');
  });

  test('going up does not lose the changes', async () => {
    const { run, type, area, root } = await mounted({ '/d/a.txt': 'one\n' }, '/d/a.txt');
    await run('viewer:toggle-edit');
    type('mine\n');
    await run('viewer:back');
    expect(area()!.value).toBe('mine\n');
    expect(root.getElementById('path-bar')!.textContent).toContain('/d/a.txt');
  });

  test('view:refresh asks before dropping the changes', async () => {
    const { run, type, area, provided } = await mounted({ '/a.txt': 'one\n' }, '/a.txt');
    await run('viewer:toggle-edit');
    type('mine\n');

    vi.stubGlobal('confirm', vi.fn(() => false));
    await provided.refresh!();
    await flush();
    expect(area()!.value).toBe('mine\n');

    vi.stubGlobal('confirm', vi.fn(() => true));
    await provided.refresh!();
    await flush();
    expect(area()!.value).toBe('one\n');
  });

  test('viewer:focus text gives the field the keyboard, turning the box on', async () => {
    const { root, run, area } = await mounted({ '/a.txt': 'one\n' }, '/a.txt');
    await run('viewer:focus', 'text');
    expect(box(root).checked).toBe(true);
    expect(root.activeElement).toBe(area());
  });

  test('the field is the scope Ctrl+S is bound in', async () => {
    const { run, area } = await mounted({ '/a.txt': 'one\n' }, '/a.txt');
    await run('viewer:toggle-edit');
    expect(area()!.dataset.mfFocus).toBe('viewer-text');
  });
});

describe('editing:goto in a panel text area', () => {
  test('goes to the edge of the line, not of the value', async () => {
    const { gotoLineEdge } = await import('../src/lib/commands');
    const area = document.createElement('textarea');
    document.body.append(area);
    area.value = 'one\ntwo\nthree';
    area.setSelectionRange(5, 5);
    gotoLineEdge(area, 'line-end');
    expect(area.selectionStart).toBe(7);
    gotoLineEdge(area, 'line-start');
    expect(area.selectionStart).toBe(4);
  });
});
