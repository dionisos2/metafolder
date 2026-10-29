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
  return { editor, store, listeners, run, input, mf, shadow, metafolder };
}

beforeEach(() => {
  document.body.innerHTML = '';
});

describe('createQueryEditor', () => {
  test('applying the simplified zone compiles it, shows its expansion, and runs it', async () => {
    const { editor, store, run, input, mf } = setup();
    await editor.restore();
    input('query-input').value = 'jazz';
    await editor.apply(mf);
    expect(input('expansion-output').value).toBe('expanded(jazz)');
    expect(input('normal-input').value).toBe('');
    expect(editor.ir()).toEqual({ dsl: 'expanded(jazz)' });
    expect(editor.text()).toBe('expanded(jazz)');
    expect(store.get('metarecord-list:query')).toBe('jazz');
    expect(store.get('metarecord-list:base-query-text')).toBe('expanded(jazz)');
    expect(run).toHaveBeenCalledTimes(1);
  });

  test('the expansion line can be copied but not edited', async () => {
    const { editor, input } = setup();
    await editor.restore();
    expect(input('expansion-output').readOnly).toBe(true);
    expect(input('normal-input').readOnly).toBe(false);
  });

  test('there is no Freeze any more', async () => {
    const { shadow } = setup();
    expect(shadow.getElementById('normal-freeze')).toBeNull();
  });

  test('the normal DSL alone is the query', async () => {
    const { editor, input, mf } = setup();
    await editor.restore();
    input('normal-input').value = 'rating > 3';
    await editor.apply(mf);
    expect(editor.ir()).toEqual({ dsl: 'rating > 3' });
    expect(editor.text()).toBe('rating > 3');
  });

  test('both zones are a conjunction: <simplified> AND <normal>', async () => {
    const { editor, store, input, mf } = setup();
    await editor.restore();
    input('query-input').value = 'jazz';
    input('normal-input').value = 'rating > 3';
    await editor.apply(mf);
    expect(editor.ir()).toEqual({
      type: 'and',
      operands: [{ dsl: 'expanded(jazz)' }, { dsl: 'rating > 3' }],
    });
    expect(editor.text()).toBe('(expanded(jazz)) AND (rating > 3)');
    expect(store.get('metarecord-list:base-query-text')).toBe('(expanded(jazz)) AND (rating > 3)');
    expect(store.get('metarecord-list:normal-query')).toBe('rating > 3');
  });

  test('a parse error is shown under the zone it comes from', async () => {
    const { editor, input, mf, run, metafolder } = setup();
    metafolder.query.parse = async (dsl: string) => {
      if (dsl.includes('bad')) throw new Error(`cannot parse ${dsl}`);
      return { dsl };
    };
    await editor.restore();
    input('query-input').value = 'jazz';
    input('normal-input').value = 'bad';
    await editor.apply(mf);
    expect(input('normal-error').textContent).toBe('cannot parse bad');
    expect(input('query-error').textContent).toBe('');
    expect(run).not.toHaveBeenCalled();
  });

  test('a shown query replaces both zones by default', async () => {
    const { editor, store, run, input, mf } = setup({ 'metarecord-list:query': 'jazz' });
    await editor.restore();
    await editor.show(mf, 'orphan = true');
    expect(input('query-input').value).toBe('');
    expect(input('normal-input').value).toBe('orphan = true');
    expect(store.get('metarecord-list:query')).toBe('');
    expect(store.get('metarecord-list:normal-query')).toBe('orphan = true');
    expect(editor.ir()).toEqual({ dsl: 'orphan = true' });
    expect(run).toHaveBeenCalledTimes(1);
  });

  test('a shown query `and` keeps the simplified query and replaces the normal DSL', async () => {
    const { editor, input, mf } = setup({
      'metarecord-list:query': 'jazz',
      'metarecord-list:normal-query': 'x = 1',
    });
    await editor.restore();
    await editor.show(mf, 'orphan = true', 'and');
    expect(input('query-input').value).toBe('jazz');
    expect(input('normal-input').value).toBe('orphan = true');
    expect(editor.text()).toBe('(expanded(jazz)) AND (orphan = true)');
  });

  test('a shown query with another mode is an error', async () => {
    const { editor, mf } = setup();
    await editor.restore();
    await expect(editor.show(mf, 'x = 1', 'or')).rejects.toThrow(/replace \/ and/);
  });

  test('the arrow hides and shows the expansion, and the choice is kept', async () => {
    const { editor, store, input, shadow, mf } = setup();
    await editor.restore();
    const row = shadow.getElementById('expansion-row')!;
    expect(row.hidden).toBe(false);
    input('expansion-toggle').click();
    await vi.waitFor(() => expect(store.get('metarecord-list:show-expansion')).toBe(false));
    expect(row.hidden).toBe(true);
    await editor.toggleExpansion(mf);
    expect(row.hidden).toBe(false);
    expect(store.get('metarecord-list:show-expansion')).toBe(true);
  });

  test('a stored choice to hide the expansion is restored', async () => {
    const { editor, shadow } = setup({ 'metarecord-list:show-expansion': false });
    await editor.restore();
    expect(shadow.getElementById('expansion-row')!.hidden).toBe(true);
  });

  test('a write of its variables by someone else is followed', async () => {
    const { editor, store, listeners, run, input } = setup();
    await editor.restore();
    store.set('metarecord-list:query', 'rock');
    listeners.get('metarecord-list:query')!('rock');
    await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(1));
    expect(input('query-input').value).toBe('rock');
    expect(editor.ir()).toEqual({ dsl: 'expanded(rock)' });
    store.set('metarecord-list:normal-query', 'y = 2');
    listeners.get('metarecord-list:normal-query')!('y = 2');
    await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(2));
    expect(input('normal-input').value).toBe('y = 2');
    expect(editor.text()).toBe('(expanded(rock)) AND (y = 2)');
    editor.dispose();
  });

  test('an empty query matches everything', async () => {
    const { editor, input, mf } = setup({
      'metarecord-list:query': 'x',
      'metarecord-list:normal-query': 'y',
    });
    await editor.restore();
    await editor.clear(mf);
    expect(editor.ir()).toBeNull();
    expect(editor.text()).toBe('');
    expect(input('expansion-output').value).toBe('');
  });
});
