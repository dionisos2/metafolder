// The log panel goes into a label, the way treeref goes into a node (doc "log
// panel", doc "Opening a row"): it lists one level at a time — the units of
// work, then what one holds, down to the operations — with a breadcrumb back
// out. A row's ↘ button goes into it; a click only selects; no double click.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { sameNodes } from './support/nodes';
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

const ENTITY = 'e'.repeat(32);
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
/** What `GET /log/labels/:id?op=` answers: the operation with its snapshots. */
const DETAIL = (id: number, label: number) => ({
  label: { id: label, name: 'mf tag add', created: 0, is_head: false, operation_count: 2 },
  operations: [
    {
      id,
      op_type: 'set_field',
      entity_uuid: ENTITY,
      field_name: 'tag',
      reverts_op_id: null,
      snapshots_before: [
        { field_name: 'tag', value_type: 'string', value_text: 'old' },
        { field_name: 'rating', value_type: 'int', value_int: 3 },
      ],
      snapshots_after: [
        { field_name: 'tag', value_type: 'string', value_text: 'new' },
        { field_name: 'rating', value_type: 'int', value_int: 3 },
      ],
    },
  ],
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
    daemon: {
      call: vi.fn(async (_method: string, url: string) => {
        const detail = /\/log\/labels\/(\d+)\?op=(\d+)$/.exec(url);
        if (detail) return DETAIL(Number(detail[2]), Number(detail[1]));
        return structuredClone(LOG);
      }),
      resolvePath: vi.fn(async () => '/photos/a.jpg'),
      resolveTreeRef: vi.fn(async () => ''),
      repoRoot: vi.fn(async () => '/root'),
    },
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
    /** The operation gone into: its before/after table, one row per field. */
    detail: () =>
      [...shadow.querySelectorAll<HTMLElement>('#op-detail tbody tr')].map((tr) =>
        [...tr.querySelectorAll('td')].map((td) => td.textContent?.trim()).join(' | '),
      ),
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

  test("a label's ↘ button lists what it holds, and the breadcrumb says where", async () => {
    const p = await mountPanel();
    p.rowWith('run').querySelector<HTMLButtonElement>('button.mf-open-enter')!.click();
    await settle();
    expect(p.rows()).toEqual(['#2 answer']);
    expect(p.crumbs()).toContain('#1 run');
  });

  test("down to an operation, whose ↘ button shows what it changed, field by field", async () => {
    const p = await mountPanel();
    for (const name of ['run', 'answer', 'mf tag add', 'op 10']) {
      p.rowWith(name).querySelector<HTMLButtonElement>('button.mf-open-enter')!.click();
      await settle();
    }
    expect(p.api.daemon.call).toHaveBeenCalledWith('GET', '/repos/r/log/labels/3?op=10');
    expect(p.rows()).toEqual([]);
    expect(p.crumbs()).toContain('op 10');
    // The unchanged field is hidden until asked for.
    expect(p.detail()).toEqual(['tag | old | new']);
    expect(p.shadow.getElementById('op-detail')!.textContent).toContain('/photos/a.jpg');
    await p.run('log:toggle', 'unchanged');
    expect(p.detail()).toEqual(['tag | old | new', 'rating | 3 | 3']);
  });

  test('cursor:parent comes back out of an operation onto its row', async () => {
    const p = await mountPanel();
    await p.run('cursor:first'); // #9 edit
    await p.run('cursor:descend');
    await p.run('cursor:descend'); // op 12
    expect(p.detail()).toEqual(['tag | old | new']);
    await p.run('cursor:parent');
    expect(p.shadow.getElementById('op-detail')!.hidden).toBe(true);
    expect(p.selected()).toBe('op 12: set_field tag');
  });

  // A held arrow key: the rows stay, only the highlight moves — the log
  // rebuilt its whole table (up to a thousand rows) on every key, and the
  // events piled up (doc "Moving the cursor").
  test('moving the cursor keeps the rows', async () => {
    const p = await mountPanel();
    const before = [...p.shadow.querySelectorAll('#rows tr')];
    await p.run('cursor:next');
    await p.run('cursor:next');
    expect(sameNodes([...p.shadow.querySelectorAll('#rows tr')], before)).toBe(true);
    expect(p.selected()).toBe('#1 run');
    expect(p.shadow.querySelectorAll('#rows tr.selected')).toHaveLength(1);
  });

  // The list scrolls inside the panel (with the wheel and a scrollbar) rather
  // than overflowing it: its container takes the room left and scrolls.
  test('the list container scrolls', () => {
    const html = readFileSync(resolve(PANEL_DIR, 'index.html'), 'utf8');
    const rule = /#revisions\s*\{([^}]*)\}/.exec(html)?.[1] ?? '';
    expect(rule).toMatch(/overflow-y:\s*auto/);
    expect(rule).toMatch(/flex:\s*1/);
    expect(rule).toMatch(/min-height:\s*0/);
  });

  test('reverting while an operation is shown reverts that operation', async () => {
    const p = await mountPanel();
    await p.run('cursor:first');
    await p.run('cursor:descend');
    await p.run('cursor:descend'); // op 12
    p.api.daemon.call.mockResolvedValueOnce({ operations: [] } as never); // the plan: nothing
    await p.run('log:revert');
    expect(p.api.daemon.call).toHaveBeenCalledWith('GET', '/repos/r/revert/plan?target_op_ids=12');
  });

  test('cursor:descend goes into the selected row; cursor:parent comes back onto it', async () => {
    const p = await mountPanel();
    await p.run('cursor:last'); // #1 run
    await p.run('cursor:descend');
    expect(p.rows()).toEqual(['#2 answer']);
    expect(p.selected()).toBe('#2 answer');
    await p.run('cursor:parent');
    expect(p.rows()).toEqual(['#9 edit', '#1 run']);
    expect(p.selected()).toBe('#1 run');
  });

  test('a crumb goes back to its level', async () => {
    const p = await mountPanel();
    await p.run('cursor:last');
    await p.run('cursor:descend');
    await p.run('cursor:descend');
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
