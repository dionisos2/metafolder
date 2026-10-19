// The list's two-zone query editor driven on its own: the zones, the query
// they compile to, and the workspace variables that are its state. Running
// the query is the panel's, reached through the context.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { createQueryEditor } from '../../default-config/panel-types/metarecord-list/query-editor.js';

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

function setup(vars: Record<string, unknown> = {}) {
  const shadow = shadowFor();
  const store = new Map<string, unknown>(Object.entries(vars));
  const listeners = new Map<string, (value: unknown) => void>();
  const metafolder = {
    settings: {},
    whenVisible: (fn: () => void) => fn(),
    daemon: { setContext: vi.fn() },
    query: {
      expand: async (text: string) => `expanded(${text})`,
      parse: async (dsl: string) => ({ dsl }),
    },
    workspace: {
      get: async (key: string) => store.get(key) ?? null,
      set: async (key: string, value: unknown) => void store.set(key, value),
      onChange: (key: string, fn: (value: unknown) => void) => void listeners.set(key, fn),
    },
  };
  const run = vi.fn(async () => {});
  const editor = createQueryEditor({
    root: shadow,
    metafolder: metafolder as never,
    history: { push: () => {} },
    onApply: () => {},
    run,
  });
  const input = (id: string) => shadow.getElementById(id) as HTMLInputElement;
  // What the editor writes through: the panel's API, or a command's run.
  const mf = metafolder as never;
  return { editor, store, listeners, run, input, mf };
}

beforeEach(() => {
  document.body.innerHTML = '';
});

describe('createQueryEditor', () => {
  test('applying the simplified zone compiles it, mirrors it in B, and runs it', async () => {
    const { editor, store, run, input, mf } = setup();
    await editor.restore();
    input('query-input').value = 'jazz';
    await editor.apply(mf);
    expect(input('normal-input').value).toBe('expanded(jazz)');
    expect(editor.ir()).toEqual({ dsl: 'expanded(jazz)' });
    expect(editor.text()).toBe('expanded(jazz)');
    expect(store.get('metarecord-list:query')).toBe('jazz');
    expect(store.get('metarecord-list:base-query-text')).toBe('expanded(jazz)');
    expect(run).toHaveBeenCalledTimes(1);
  });

  test('a shown query is frozen in the normal zone and run as typed', async () => {
    const { editor, store, run, input, mf } = setup();
    await editor.restore();
    await editor.show(mf, 'orphan = true');
    expect(input('normal-input').value).toBe('orphan = true');
    expect(input('normal-freeze').checked).toBe(true);
    expect(store.get('metarecord-list:normal-frozen')).toBe(true);
    expect(editor.ir()).toEqual({ dsl: 'orphan = true' });
    expect(run).toHaveBeenCalledTimes(1);
  });

  test('a write of its variables by someone else is followed', async () => {
    const { editor, store, listeners, run, input } = setup();
    await editor.restore();
    store.set('metarecord-list:query', 'rock');
    listeners.get('metarecord-list:query')!('rock');
    await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(1));
    expect(input('query-input').value).toBe('rock');
    expect(editor.ir()).toEqual({ dsl: 'expanded(rock)' });
    editor.dispose();
  });

  test('an empty query matches everything', async () => {
    const { editor, mf } = setup({ 'metarecord-list:query': 'x' });
    await editor.restore();
    await editor.clear(mf);
    expect(editor.ir()).toBeNull();
    expect(editor.text()).toBe('');
  });
});
