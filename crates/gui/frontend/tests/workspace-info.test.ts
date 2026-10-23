// workspace-info panel: it shows every variable of its workspace — including
// the ones set before the panel was built, which the change events alone never
// told it about (a panel is built when its workspace first shows it, long after
// the list wrote its query there).

import { describe, expect, test } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/workspace-info');

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

type Listener = (value: unknown, key?: string) => void;

async function mountWith(vars: Record<string, unknown>, globals: Record<string, unknown> = {}) {
  const listeners: Listener[] = [];
  const globalListeners: Listener[] = [];
  const api = {
    global: {
      get: async (key: string) => globals[key] ?? null,
      all: async () => ({ ...globals }),
      onChange: (key: string, listener: Listener) => {
        if (key === '*') globalListeners.push(listener);
      },
    },
    workspace: {
      get: async (key: string) => vars[key] ?? null,
      all: async () => ({ ...vars }),
      onChange: (key: string, listener: Listener) => {
        if (key === '*') listeners.push(listener);
      },
    },
  };
  const root = shadowRoot();
  const mod = await import('../../default-config/panel-types/workspace-info/main.js');
  await mod.mount(root, api as never);
  const rows = (table = 'vars') =>
    Object.fromEntries(
      [...root.getElementById(table)!.querySelectorAll('tr')].map((tr) => [
        tr.querySelector('.key')?.textContent ?? '',
        tr.querySelector('.value')?.textContent ?? '',
      ]),
    );
  const keys = (table = 'vars') =>
    [...root.getElementById(table)!.querySelectorAll('tr .key')].map((td) => td.textContent);
  return {
    rows,
    keys,
    push: (key: string, value: unknown) => listeners.forEach((l) => l(value, key)),
    pushGlobal: (key: string, value: unknown) => globalListeners.forEach((l) => l(value, key)),
  };
}

describe('workspace-info', () => {
  test('a variable set before the panel was built is listed', async () => {
    const { rows } = await mountWith({
      active_repo: 'repo-1',
      'metarecord-list:query': 'rating > 3',
    });
    expect(rows()['active_repo']).toBe('"repo-1"');
    expect(rows()['metarecord-list:query']).toBe('"rating > 3"');
  });

  test('the standard variables come first, unset ones as null', async () => {
    const { keys, rows } = await mountWith({ 'path-ref:scope': 'exact', active_repo: 'r' });
    expect(keys().slice(0, 5)).toEqual([
      'active_repo',
      'selected_paths',
      'selected_metarecord',
      'selected_metarecords',
      'selected_path_ref',
    ]);
    expect(rows()['selected_metarecord']).toBe('null');
    expect(keys().at(-1)).toBe('path-ref:scope');
  });

  test('a later change updates its row', async () => {
    const { rows, push } = await mountWith({ 'path-ref:scope': 'exact' });
    push('path-ref:scope', 'subtree');
    expect(rows()['path-ref:scope']).toBe('"subtree"');
  });

  test('the global GUI variables have their own section, the clipboard first', async () => {
    const { keys, rows } = await mountWith({ active_repo: 'r' }, { 'my-script:state': 1 });
    expect(keys('globals')).toEqual(['clipboard', 'my-script:state']);
    expect(rows('globals')['clipboard']).toBe('null');
    // Not a variable of the workspace.
    expect(keys()).not.toContain('clipboard');
  });

  test('a global variable set from elsewhere updates its row', async () => {
    const { rows, pushGlobal } = await mountWith({});
    pushGlobal('clipboard', { paths: ['/a'], mode: 'cut' });
    expect(JSON.parse(rows('globals')['clipboard'])).toEqual({ paths: ['/a'], mode: 'cut' });
    expect(rows()['clipboard']).toBeUndefined();
  });
});
