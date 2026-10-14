// The list panel's bulk form, driven on its own: everything it needs from the
// panel arrives through its context (the repository, the effective query), so
// it is testable without mounting the list. The panel-level behaviour is in
// metarecord-list-bulk-target.test.ts.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { createBulkEdit } from '../../default-config/panel-types/metarecord-list/bulk-edit.js';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/metarecord-list');

function shadowFor(): ShadowRoot {
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

type Call = { method: string; path: string; body: unknown };

function stubMetafolder(calls: Call[]) {
  const store = new Map<string, unknown>();
  return {
    daemon: {
      call: async (method: string, path: string, body: unknown) => {
        calls.push({ method, path, body });
        if (body && (body as { count?: boolean }).count === true) return { total: 2 };
        return { updated: 2 };
      },
    },
    workspace: {
      get: async (key: string) => store.get(key) ?? null,
      set: async (key: string, value: unknown) => void store.set(key, value),
    },
    statusBar: { message: vi.fn(async () => {}) },
    trash: { trashQuery: async () => ({}) },
    pick: { start: async () => '' },
    commands: { invoke: () => null },
  };
}

beforeEach(() => {
  document.body.innerHTML = '';
  vi.stubGlobal('confirm', vi.fn(() => true));
});

describe('createBulkEdit', () => {
  test('writes over the effective query its context provides', async () => {
    const shadow = shadowFor();
    const calls: Call[] = [];
    const bulk = createBulkEdit({
      root: shadow,
      metafolder: stubMetafolder(calls) as never,
      getRepo: () => 'r',
      effectiveQuery: () => ({ dsl: 'year = 1999' }),
      statusMessageMs: 10,
    });
    bulk.toggle();
    expect(shadow.getElementById('bulk-form')!.classList.contains('open')).toBe(true);
    (shadow.getElementById('bulk-name') as HTMLInputElement).value = 'rating';
    await bulk.apply();
    const post = calls.find((c) => c.path === '/repos/r/query/fields/set');
    expect(post?.body).toMatchObject({ query: { dsl: 'year = 1999' }, name: 'rating' });
    expect(shadow.getElementById('bulk-form')!.classList.contains('open')).toBe(false);
  });

  test('refuses to run without a repository', async () => {
    const shadow = shadowFor();
    const calls: Call[] = [];
    const bulk = createBulkEdit({
      root: shadow,
      metafolder: stubMetafolder(calls) as never,
      getRepo: () => null,
      effectiveQuery: () => null,
      statusMessageMs: 10,
    });
    (shadow.getElementById('bulk-name') as HTMLInputElement).value = 'rating';
    await bulk.apply();
    expect(calls).toEqual([]);
    expect(shadow.getElementById('bulk-error')!.textContent).toBe('no active repository');
  });
});
