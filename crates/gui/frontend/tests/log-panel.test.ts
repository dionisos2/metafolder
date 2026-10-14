// The log panel goes into a label, the way treeref goes into a node (doc "log
// panel", doc "Opening a row"): it lists one level at a time — the units of
// work, then what one holds, down to the operations — with a breadcrumb back
// out. A row's › button goes into it; a click only selects; no double click.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { generic } from './support/cursor';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/log');

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

type Handler = (...args: string[]) => unknown;

const op = (id: number, label: number) => ({
  id,
  label_id: label,
  op_type: 'set_field',
  origin: null,
  reverts_op_id: null,
  timestamp: id * 1000,
  field_name: 'tag',
  entity_uuid: null,
});
const label = (id: number, name: string, parent: number | null = null) => ({
  id,
  name,
  description: null,
  parent,
  created: 0,
});
// A script's run (#1) holding one answer (#2) holding one command (#3), and a
// command of its own (#9).
const LOG = {
  operations: [op(10, 3), op(11, 3), op(12, 9)],
  labels: [label(1, 'run'), label(2, 'answer', 1), label(3, 'mf tag add', 2), label(9, 'edit')],
  head: 12,
};

async function mountPanel() {
  const shadow = shadowFor();
  const handlers = new Map<string, Handler>();
  const noop = () => {};
  const api = {
    settings: {},
    whenVisible: (fn: () => void) => fn(),
    daemon: { call: vi.fn(async () => structuredClone(LOG)) },
    workspace: {
      get: async (key: string) => (key === 'active_repo' ? 'r' : null),
      set: vi.fn(async () => {}),
      onChange: noop,
    },
    provide: vi.fn(),
    commands: {
      register: async (name: string, opts: { handler?: Handler }) => {
        if (opts.handler) handlers.set(name, opts.handler);
        return null;
      },
      invoke: vi.fn(),
    },
    statusBar: { message: vi.fn(async () => {}), error: vi.fn(async () => {}) },
    log: { revert: vi.fn() },
  };
  const mod = await import('../../default-config/panel-types/log/main.js');
  await mod.mount(shadow, api as never);
  await settle();
  const rowWith = (text: string) =>
    [...shadow.querySelectorAll<HTMLElement>('#rows tr')].find((tr) => tr.textContent?.includes(text))!;
  return {
    shadow,
    api,
    run: async (name: string, ...args: string[]) => {
      // A generic command runs on what the panel provided (doc "Generic panel commands").
      if (name.startsWith('cursor:')) await generic(api.provide, name, ...args);
      else await handlers.get(name)!(...args);
      await settle();
    },
    /** The first cell of each row, without its button. */
    rows: () =>
      [...shadow.querySelectorAll<HTMLElement>('#rows tr')].map((tr) =>
        tr.querySelector('td.name')!.textContent.trim(),
      ),
    rowWith,
    crumbs: () => shadow.getElementById('breadcrumb')!.textContent ?? '',
    selected: () => shadow.querySelector('#rows tr.selected td.name')?.textContent?.trim() ?? null,
  };
}

async function settle() {
  for (let i = 0; i < 5; i += 1) await new Promise((r) => setTimeout(r, 0));
}

describe('log panel: going into a label', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {};
    document.body.replaceChildren();
  });

  test('the top level lists the units of work, newest first', async () => {
    const p = await mountPanel();
    expect(p.rows()).toEqual(['#9 edit', '#1 run']);
  });

  test("a label's › button lists what it holds, and the breadcrumb says where", async () => {
    const p = await mountPanel();
    p.rowWith('run').querySelector<HTMLButtonElement>('button.mf-open-enter')!.click();
    await settle();
    expect(p.rows()).toEqual(['#2 answer']);
    expect(p.crumbs()).toContain('#1 run');
  });

  test('down to the operations, which have no button: they hold nothing', async () => {
    const p = await mountPanel();
    for (const name of ['run', 'answer', 'mf tag add']) {
      p.rowWith(name).querySelector<HTMLButtonElement>('button.mf-open-enter')!.click();
      await settle();
    }
    expect(p.rows()).toEqual(['op 11: set_field tag', 'op 10: set_field tag']);
    expect(p.shadow.querySelector('#rows button.mf-open')).toBeNull();
  });

  test('log:descend goes into the selected row; cursor:parent comes back onto it', async () => {
    const p = await mountPanel();
    await p.run('cursor:last'); // #1 run
    await p.run('log:descend');
    expect(p.rows()).toEqual(['#2 answer']);
    expect(p.selected()).toBe('#2 answer');
    await p.run('cursor:parent');
    expect(p.rows()).toEqual(['#9 edit', '#1 run']);
    expect(p.selected()).toBe('#1 run');
  });

  test('a crumb goes back to its level', async () => {
    const p = await mountPanel();
    await p.run('cursor:last');
    await p.run('log:descend');
    await p.run('log:descend');
    expect(p.rows()).toEqual(['#3 mf tag add']);
    const top = p.shadow.querySelector<HTMLElement>('#breadcrumb .crumb')!;
    top.click();
    await settle();
    expect(p.rows()).toEqual(['#9 edit', '#1 run']);
  });

  test('a click selects and a double click goes nowhere', async () => {
    const p = await mountPanel();
    p.rowWith('run').click();
    await settle();
    expect(p.selected()).toBe('#1 run');
    p.rowWith('run').dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
    await settle();
    expect(p.rows()).toEqual(['#9 edit', '#1 run']);
  });

  test('reverting one operation from a label goes to where that operation is listed', async () => {
    const p = await mountPanel();
    await p.run('cursor:first'); // #9 edit: one operation
    p.api.daemon.call.mockResolvedValueOnce({ operations: [] } as never); // the plan: nothing
    await p.run('log:revert', 'op');
    expect(p.rows()).toEqual(['op 12: set_field tag']);
    expect(p.selected()).toBe('op 12: set_field tag');
    expect(p.crumbs()).toContain('#9 edit');
  });
});
