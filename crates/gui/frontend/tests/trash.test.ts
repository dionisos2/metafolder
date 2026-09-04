// trash panel: format helpers plus the mount/action flow (list, restore,
// delete, empty) driven through a stub metafolder API. Mirrors the shell's
// mount path (Shadow root + index.html markup) so the id lookups are real.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { formatAge, formatSize, mount } from '../../default-config/panel-types/trash/main.js';

describe('trash format helpers', () => {
  test('formatSize scales by 1024', () => {
    expect(formatSize(512)).toBe('512B');
    expect(formatSize(2048)).toBe('2.0K');
    expect(formatSize(5 * 1024 * 1024)).toBe('5.0M');
    expect(formatSize(3 * 1024 ** 3)).toBe('3.0G');
  });

  test('formatAge picks the coarsest unit', () => {
    const now = 1_000_000_000_000;
    expect(formatAge(now - 5_000, now)).toBe('5s ago');
    expect(formatAge(now - 120_000, now)).toBe('2m ago');
    expect(formatAge(now - 3 * 3_600_000, now)).toBe('3h ago');
    expect(formatAge(now - 2 * 86_400_000, now)).toBe('2d ago');
    // A clock skew (future timestamp) never goes negative.
    expect(formatAge(now + 10_000, now)).toBe('0s ago');
  });
});

/** The shell's mount path: index.html body (minus scripts/styles) into a Shadow root. */
function shadowForTrash(): ShadowRoot {
  const html = readFileSync(
    resolve(process.cwd(), '../default-config/panel-types/trash/index.html'),
    'utf8',
  );
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

const ENTRIES = [
  {
    id: 'a',
    original_path: '/r/x.txt',
    original_name: 'x.txt',
    trashed_at: Date.now() - 5_000,
    size: 2048,
    is_dir: false,
    reason: 'manual' as const,
  },
  {
    id: 'b',
    original_path: '/r/dir',
    original_name: 'dir',
    trashed_at: Date.now() - 2 * 86_400_000,
    size: 100,
    is_dir: true,
    reason: 'rollback' as const,
  },
];

/** A stub metafolder whose `whenVisible` fires immediately and whose command
 *  handlers are captured for the test to invoke. */
function stub() {
  /** @type {Map<string, (...a: string[]) => unknown>} */
  const handlers = new Map<string, (...a: string[]) => unknown>();
  const trash = {
    list: vi.fn(async () => ENTRIES.slice()),
    restore: vi.fn(async (_repo: string, _id: string) => '/r/x.txt'),
    remove: vi.fn(async () => {}),
    empty: vi.fn(async () => 2),
  };
  const workspace = { get: vi.fn(async () => 'repo-1'), set: vi.fn(async () => {}), onChange: vi.fn() };
  const statusBar = { message: vi.fn(async () => {}), error: vi.fn(async () => {}) };
  const api = {
    settings: {},
    defaults: {},
    trash,
    workspace,
    statusBar,
    commands: {
      register: vi.fn((name: string, opts: { handler?: (...a: string[]) => unknown }) => {
        if (opts.handler) handlers.set(name, opts.handler);
      }),
      invoke: vi.fn(),
    },
    contextMenu: Object.assign(vi.fn(), { addDefaultItems: vi.fn() }),
    whenVisible: (fn: () => void) => fn(),
  };
  return { api, handlers, trash, workspace, statusBar };
}

describe('trash panel actions', () => {
  beforeEach(() => {
    vi.stubGlobal('confirm', vi.fn(() => true));
    // jsdom does not implement scrollIntoView, which select() calls.
    if (!Element.prototype.scrollIntoView) Element.prototype.scrollIntoView = () => {};
  });

  test('mount lists entries and renders one row per entry', async () => {
    const { api, trash } = stub();
    const shadow = shadowForTrash();
    mount(shadow, api as never);
    // `whenVisible` fired start(), but its load() is async: let it settle.
    await new Promise((r) => setTimeout(r, 0));
    expect(trash.list).toHaveBeenCalledWith('repo-1');
    expect(shadow.querySelectorAll('#entries > li')).toHaveLength(ENTRIES.length);
    // Right-clicking a row opens the context menu (rowMenu → contextMenu).
    const firstRow = shadow.querySelector('#entries > li') as HTMLElement;
    firstRow.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true }));
    expect(api.contextMenu).toHaveBeenCalled();
  });

  test('restore, delete and empty drive the trash API', async () => {
    const { api, handlers, trash, workspace, statusBar } = stub();
    mount(shadowForTrash(), api as never);
    // mount is synchronous, but its whenVisible→load() is async: let it settle
    // so the entries are present before the cursor/restore commands run.
    await new Promise((r) => setTimeout(r, 0));

    // Move the cursor onto the first entry, then restore it.
    await handlers.get('trash:next')!();
    await handlers.get('trash:restore')!();
    expect(trash.restore).toHaveBeenCalledWith('repo-1', 'a');
    expect(workspace.set).toHaveBeenCalledWith('metarecords:dirty', expect.any(Number));

    // Delete the (now first) entry after a confirmation.
    await handlers.get('trash:delete')!();
    expect(trash.remove).toHaveBeenCalledTimes(1);

    // Empty the whole trash.
    await handlers.get('trash:empty')!();
    expect(trash.empty).toHaveBeenCalledWith('repo-1');
    expect(statusBar.message).toHaveBeenCalled();
  });

  test('destructive actions are cancelled when unconfirmed', async () => {
    vi.stubGlobal('confirm', vi.fn(() => false));
    const { api, handlers, trash } = stub();
    mount(shadowForTrash(), api as never);
    await handlers.get('trash:next')!();
    await handlers.get('trash:delete')!();
    await handlers.get('trash:empty')!();
    expect(trash.remove).not.toHaveBeenCalled();
    expect(trash.empty).not.toHaveBeenCalled();
  });
});
