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

async function mountWith(vars: Record<string, unknown>) {
  const listeners: Listener[] = [];
  const api = {
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
  const rows = () =>
    Object.fromEntries(
      [...root.querySelectorAll('tr')].map((tr) => [
        tr.querySelector('.key')?.textContent ?? '',
        tr.querySelector('.value')?.textContent ?? '',
      ]),
    );
  const keys = () => [...root.querySelectorAll('tr .key')].map((td) => td.textContent);
  return { rows, keys, push: (key: string, value: unknown) => listeners.forEach((l) => l(value, key)) };
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
    const { keys, rows } = await mountWith({ 'treeref:scope': 'exact', active_repo: 'r' });
    expect(keys().slice(0, 6)).toEqual([
      'active_repo',
      'selected_paths',
      'selected_metarecord',
      'selected_metarecords',
      'selected_log_entry',
      'selected_treeref',
    ]);
    expect(rows()['selected_metarecord']).toBe('null');
    expect(keys().at(-1)).toBe('treeref:scope');
  });

  test('a later change updates its row', async () => {
    const { rows, push } = await mountWith({ 'treeref:scope': 'exact' });
    push('treeref:scope', 'subtree');
    expect(rows()['treeref:scope']).toBe('"subtree"');
  });
});
