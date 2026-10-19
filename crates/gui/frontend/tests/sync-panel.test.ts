// sync panel: its commands can be driven without its toolbar (doc "sync panel"):
// `sync:pair A B` chooses the pair by name or uuid, `sync:plan [intents]` takes
// the intents file as an argument, and status and plan say what they found on
// the status bar — so a script, or the GUI's end-to-end test, can run a sync.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve as resolvePath } from 'node:path';
import { withRuns } from './support/runs';

const PANEL_DIR = resolvePath(process.cwd(), '../default-config/panel-types/sync');

/** The shell's mount path: the panel's body (minus scripts/styles) into a Shadow root. */
function shadowFor(): ShadowRoot {
  const html = readFileSync(resolvePath(PANEL_DIR, 'index.html'), 'utf8');
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

const REPOS = [
  { repo_uuid: 'aaa', name: 'laptop' },
  { repo_uuid: 'bbb', name: 'nas' },
  { repo_uuid: 'ccc', name: 'phone' },
];

async function mountPanel() {
  const shadow = shadowFor();
  const handlers = new Map<string, Handler>();
  const statusBar = { message: vi.fn(async () => {}), error: vi.fn(async () => {}) };
  const sync = {
    status: vi.fn(async () => ({
      links: [
        { uuid: 'l1', state: 'in_sync' },
        { uuid: 'l2', state: 'in_sync' },
        { uuid: 'l3', state: 'changed_a' },
      ],
    })),
    plan: vi.fn(async () => ({ plan_uuid: 'p', operations: 2, warnings: ['w'] })),
    show: vi.fn(async () => ({ state: 'empty' })),
    run: vi.fn(async () => ({ status: 'ran', done: 2, skipped: 0, divergences: [], warnings: [] })),
  };
  const noop = () => {};
  const api = {
    settings: {},
    whenVisible: (fn: () => void) => fn(),
    sync,
    daemon: { call: async (_m: string, path: string) => (path.startsWith('/repos') ? REPOS : null) },
    query: { parse: async () => null },
    workspace: {
      get: async (key: string) => (key === 'active_repo' ? 'aaa' : null),
      set: async () => {},
      onChange: noop,
    },
    commands: {
      register: async (name: string, spec: { handler: Handler }) => {
        handlers.set(name, spec.handler);
        return null;
      },
    },
    statusBar,
  };
  const mod = await import('../../default-config/panel-types/sync/main.js');
  mod.mount(shadow, withRuns(api) as never);
  await new Promise((r) => setTimeout(r, 0)); // let the deferred start settle
  const invoke = async (name: string, ...args: string[]) => {
    const h = handlers.get(name);
    if (!h) throw new Error(`command not registered: ${name}`);
    await h(...args);
  };
  return { shadow, sync, statusBar, invoke };
}

describe('sync panel commands', () => {
  beforeEach(() => {
    vi.stubGlobal('confirm', vi.fn(() => true));
    document.body.replaceChildren();
  });

  test('sync:pair chooses the pair by name, and says so', async () => {
    const p = await mountPanel();
    await p.invoke('sync:pair', 'nas', 'phone');
    expect(p.statusBar.message).toHaveBeenLastCalledWith('Sync: nas ⇄ phone', expect.any(Number));
    await p.invoke('sync:status');
    expect(p.sync.status).toHaveBeenCalledWith('bbb', 'ccc');
  });

  test('sync:pair also takes uuids', async () => {
    const p = await mountPanel();
    await p.invoke('sync:pair', 'ccc', 'aaa');
    await p.invoke('sync:status');
    expect(p.sync.status).toHaveBeenCalledWith('ccc', 'aaa');
  });

  test('sync:pair refuses an unknown repository and a pair of one', async () => {
    const p = await mountPanel();
    await expect(p.invoke('sync:pair', 'nas', 'tablet')).rejects.toThrow(
      'no loaded repository named "tablet"',
    );
    await expect(p.invoke('sync:pair', 'nas', 'bbb')).rejects.toThrow(
      'a sync needs two different repositories',
    );
  });

  test('sync:status reports the links on the status bar, counted by state', async () => {
    const p = await mountPanel();
    await p.invoke('sync:status');
    expect(p.statusBar.message).toHaveBeenLastCalledWith(
      'Sync status: 3 links (2 in_sync, 1 changed_a)',
      expect.any(Number),
    );
  });

  test('sync:plan takes the intents file as its argument and reports the plan', async () => {
    const p = await mountPanel();
    await p.invoke('sync:plan', '/tmp/intents.toml');
    expect(p.sync.plan).toHaveBeenCalledWith('aaa', 'bbb', '/tmp/intents.toml');
    expect(p.statusBar.message).toHaveBeenLastCalledWith(
      'Plan: 2 operation(s) · 1 warning(s)',
      expect.any(Number),
    );
    const intents = p.shadow.getElementById('intents') as HTMLInputElement;
    expect(intents.value).toBe('/tmp/intents.toml');
  });

  test('sync:plan with no intents file at all is refused', async () => {
    const p = await mountPanel();
    await expect(p.invoke('sync:plan')).rejects.toThrow('no intents file');
    expect(p.sync.plan).not.toHaveBeenCalled();
  });
});
