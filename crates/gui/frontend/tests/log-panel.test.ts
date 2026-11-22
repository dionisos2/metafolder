// The log panel goes into a log context, the way path-ref goes into a node (doc "log
// panel", doc "Opening a row"): it lists one level at a time — the units of
// work, then what one holds, down to the operations — with a breadcrumb back
// out. A row's ↘ button goes into it; a click only selects; no double click.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { sameNodes } from './support/nodes';
import { generic } from './support/cursor';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { withRuns } from './support/runs';
import { shippedSettings } from './support/shipped-defaults';

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
const op = (id: number, logContext: number) => ({
  id,
  log_context_id: logContext,
  op_type: 'set_field',
  origin: null,
  reverts_op_id: null,
  timestamp: id * 1000,
  field_name: 'tag',
  entity_uuid: null,
});
/** What `GET /log/contexts/:id?op=` answers: the operation with its before. */
const DETAIL = (id: number, logContext: number) => ({
  log_context: { id: logContext, name: 'mf tag add', created: 0, is_head: false, operation_count: 2 },
  operations: [
    {
      id,
      op_type: 'set_field',
      entity_uuid: ENTITY,
      field_name: 'tag',
      reverts_op_id: null,
      before: [
        { field_name: 'tag', value_type: 'string', value_text: 'old' },
        { field_name: 'rating', value_type: 'int', value_int: 3 },
      ],
    },
  ],
});
/** A log context as `GET /log/contexts` summarises it (doc "Log context summaries"). */
const logContext = (id: number, name: string, extra: Record<string, unknown> = {}) => ({
  id,
  name,
  description: null,
  parent: null,
  created: 0,
  updated: id * 1000,
  operations: 0,
  children: 0,
  first_op: null,
  last_op: null,
  watcher: false,
  reverts: [],
  reverted_by: [],
  undone: null,
  is_head: false,
  ...extra,
});
// A script's run (#1) holding one answer (#2) holding one command (#3), and a
// command of its own (#9).
const UNITS = {
  head: 12,
  head_log_context: 9,
  log_contexts: [
    logContext(9, 'edit', { operations: 1, last_op: 12, is_head: true }),
    logContext(1, 'run', { children: 1 }),
  ],
  next_before: null,
  total_log_contexts: 4,
};
/** What a log context holds: the log contexts nested in it, and its own operations. */
const NESTED: Record<number, ReturnType<typeof logContext>[]> = {
  1: [logContext(2, 'answer', { parent: 1, children: 1 })],
  2: [logContext(3, 'mf tag add', { parent: 2, operations: 2, last_op: 11 })],
  3: [],
  9: [],
};
const OPS: Record<number, ReturnType<typeof op>[]> = { 3: [op(11, 3), op(10, 3)], 9: [op(12, 9)] };

async function mountPanel(units: typeof UNITS = UNITS, above: number[] = []) {
  const shadow = shadowFor();
  const handlers = new Map<string, Handler>();
  const noop = () => {};
  const api = {
    settings: shippedSettings(),
    whenVisible: (fn: () => void) => fn(),
    daemon: {
      call: vi.fn(async (_method: string, url: string) => {
        const detail = /\/log\/contexts\/(\d+)\?op=(\d+)$/.exec(url);
        if (detail) return DETAIL(Number(detail[2]), Number(detail[1]));
        const own = /\/log\/contexts\/(\d+)\?newest=1/.exec(url);
        if (own) {
          const ops = OPS[Number(own[1])] ?? [];
          return { log_context: { id: Number(own[1]), operation_count: ops.length }, operations: ops };
        }
        const nested = /\/log\/contexts\?parent=(\d+)/.exec(url);
        if (nested) return { log_contexts: NESTED[Number(nested[1])] ?? [], next_before: null };
        if (/\/contexts\/\d+$/.test(url)) {
          return { id: 9, ancestors: above.map((id) => ({ id })), children: [] };
        }
        return structuredClone(units);
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
  await mod.mount(shadow, withRuns(api) as never);
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
    /** The operation gone into: the rows it took off, one row per field. */
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

describe('log panel: going into a log context', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {};
    document.body.replaceChildren();
  });

  test('the top level lists the units of work, newest first', async () => {
    const p = await mountPanel();
    expect(p.rows()).toEqual(['#9 edit', '#1 run']);
  });

  test("a log context's ↘ button lists what it holds, and the breadcrumb says where", async () => {
    const p = await mountPanel();
    p.rowWith('run').querySelector<HTMLButtonElement>('button.mf-open-enter')!.click();
    await settle();
    expect(p.rows()).toEqual(['#2 answer']);
    expect(p.crumbs()).toContain('#1 run');
  });

  test("down to an operation, whose ↘ button shows the rows it took off, field by field", async () => {
    const p = await mountPanel();
    for (const name of ['run', 'answer', 'mf tag add', 'op 10']) {
      p.rowWith(name).querySelector<HTMLButtonElement>('button.mf-open-enter')!.click();
      await settle();
    }
    expect(p.api.daemon.call).toHaveBeenCalledWith('GET', '/repos/r/log/contexts/3?op=10');
    expect(p.rows()).toEqual([]);
    expect(p.crumbs()).toContain('op 10');
    // The log keeps what the operation replaced, not what it left.
    expect(p.detail()).toEqual(['tag | old', 'rating | 3']);
    expect(p.shadow.getElementById('op-detail')!.textContent).toContain('/photos/a.jpg');
  });

  test('cursor:parent comes back out of an operation onto its row', async () => {
    const p = await mountPanel();
    await p.run('cursor:first'); // #9 edit
    await p.run('cursor:descend');
    await p.run('cursor:descend'); // op 12
    expect(p.detail()).toEqual(['tag | old', 'rating | 3']);
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

  test('reverting one operation from a log context goes to where that operation is listed', async () => {
    const p = await mountPanel();
    await p.run('cursor:first'); // #9 edit: one operation
    p.api.daemon.call.mockResolvedValueOnce({ operations: [] } as never); // the plan: nothing
    await p.run('log:revert', 'op');
    expect(p.rows()).toEqual(['op 12: set_field tag']);
    expect(p.selected()).toBe('op 12: set_field tag');
    expect(p.crumbs()).toContain('#9 edit');
  });
});

describe('log panel: after a revert', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {};
    document.body.replaceChildren();
  });

  test('the other panels are nudged through the workspace variable, not by a command', async () => {
    const p = await mountPanel();
    vi.stubGlobal('confirm', () => true);
    await p.run('cursor:first');
    p.api.daemon.call.mockResolvedValueOnce({ operations: [{ id: 12 }] } as never); // the plan
    p.api.log.revert.mockResolvedValueOnce({ log_context: 20, reverted_operations: [12], notes: [] });
    await p.run('log:revert', 'log-context');
    expect(p.api.log.revert).toHaveBeenCalled();
    expect(p.api.workspace.set).toHaveBeenCalledWith('metarecords:dirty', expect.any(Number));
    expect(p.api.commands.invoke).not.toHaveBeenCalledWith(['metarecords:dirty']);
    vi.unstubAllGlobals();
  });
});

describe('log panel: no operation is read until a log context is gone into', () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = () => {};
    document.body.replaceChildren();
  });

  test('the top level asks for log context summaries only', async () => {
    const p = await mountPanel();
    const urls = p.api.daemon.call.mock.calls.map((c) => c[1]);
    expect(urls).toContain('/repos/r/log/contexts?limit=100');
    expect(urls.some((u) => /\/log\?|\/log\/contexts\/\d/.test(u))).toBe(false);
  });

  test('going into a log context reads what it holds: its log contexts and the newest of its operations', async () => {
    const p = await mountPanel();
    p.rowWith('edit').querySelector<HTMLButtonElement>('button.mf-open-enter')!.click();
    await settle();
    const urls = p.api.daemon.call.mock.calls.map((c) => c[1]);
    expect(urls).toContain('/repos/r/log/contexts?parent=9&limit=500');
    expect(urls).toContain('/repos/r/log/contexts/9?newest=1&limit=500&before=0');
    expect(p.rows()).toEqual(['op 12: set_field tag']);
  });

  test('a unit says what the daemon says of it: HEAD, undone, what it undid', async () => {
    const p = await mountPanel({
      ...UNITS,
      log_contexts: [
        logContext(12, 'undo 9', {
          operations: 1,
          reverts: [{ log_context: 9, whole: true }],
          is_head: true,
        }),
        logContext(9, 'edit', {
          operations: 1,
          undone: 'fully',
          reverted_by: [{ log_context: 12, whole: true }],
        }),
      ],
      head_log_context: 12,
    });
    const badges = (name: string) =>
      [...p.rowWith(name).querySelectorAll('.badge')].map((b) => b.textContent);
    expect(badges('undo 9')).toEqual(['HEAD', 'undo of #9']);
    expect(badges('edit')).toEqual(['undone by #12']);
    expect(p.rowWith('edit').classList.contains('undone-row')).toBe(true);
  });

  test('a unit holding the HEAD operation in a nested log context is marked HEAD', async () => {
    // The HEAD operation is in log context #3, nested in #2, nested in #1.
    const p = await mountPanel({ ...UNITS, head_log_context: 3 }, [2, 1]);
    const badges = (name: string) =>
      [...p.rowWith(name).querySelectorAll('.badge')].map((b) => b.textContent);
    expect(badges('run')).toContain('HEAD');
    expect(badges('edit')).not.toContain('HEAD');
  });
});
