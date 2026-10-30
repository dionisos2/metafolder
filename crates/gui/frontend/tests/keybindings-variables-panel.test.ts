// keybindings-variables panel (doc "keybindings-variables panel"): the variables
// of keybindings.toml, each with its value, the keys it stands for and its uses.

import { describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/keybindings-variables');

const VARIABLES = [
  { name: 'go', value: 'g', keys: 'g', uses: 1 },
  { name: 'panels', value: 't', keys: 't', uses: 1 },
  { name: 'negative', value: '{tags} -', keys: ', -', uses: 3 },
  { name: 'tags', value: 'ctrl+shift+,', keys: 'ctrl+shift+,', uses: 2 },
];

const flush = async () => {
  for (let i = 0; i < 10; i++) await new Promise((r) => setTimeout(r, 0));
};

async function mountPanel(checkVariable: (name: string, value: string) => string) {
  const html = readFileSync(resolve(PANEL_DIR, 'index.html'), 'utf8');
  const doc = new DOMParser().parseFromString(html, 'text/html');
  const host = document.createElement('div');
  document.body.append(host);
  const root = host.attachShadow({ mode: 'open' });
  for (const child of [...doc.body.childNodes]) {
    if (child.nodeName !== 'STYLE') root.append(child);
  }
  const view = { entries: [], variables: VARIABLES, unbound: [], classes_error: null };
  const handlers = new Map<string, (...args: unknown[]) => unknown>();
  const api = {
    commands: {
      register: vi.fn(async (name: string, spec: { handler: (...args: unknown[]) => unknown }) => {
        handlers.set(name, spec.handler);
      }),
    },
    config: {
      keybindingEntries: vi.fn(async () => view),
      checkKeybindingVariable: vi.fn(async (name: string, value: string) => checkVariable(name, value)),
      setKeybindingVariable: vi.fn(async () => view),
    },
    workspace: { set: vi.fn(async () => {}), onChange: vi.fn() },
    visible: false,
    onVisibility: vi.fn(),
    provide: vi.fn(),
  };
  const mod = await import('../../default-config/panel-types/keybindings-variables/main.js');
  await mod.mount(root, api as never);
  await flush();
  return { root, api, handlers };
}

describe('keybindings-variables panel', () => {
  test('each variable is listed with its keys; a new value is checked, then saved on Enter', async () => {
    const { root, api } = await mountPanel((_name, value) => {
      if (value === 'x') throw new Error('`x` (`command-input:focus command`) would hide `x l`');
      return '3 keys change.';
    });
    const type = async (input: HTMLInputElement, text: string) => {
      input.value = text;
      input.dispatchEvent(new Event('input'));
      await flush();
    };
    const key = async (input: HTMLInputElement, name: string) => {
      input.dispatchEvent(new KeyboardEvent('keydown', { key: name }));
      await flush();
    };
    const variables = [...root.querySelectorAll('#variables tr')];
    expect(variables).toHaveLength(4);
    const negative = variables.find((tr) => tr.textContent.includes('negative'))!;
    expect(negative.querySelector('input')!.value).toBe('{tags} -');
    expect(negative.textContent).toContain(', -');
    expect(negative.textContent).toContain('3');

    const go = variables.find((tr) => tr.textContent.includes('go'))!.querySelector('input')!;
    await type(go, 'x');
    expect(root.getElementById('variable-check')!.textContent).toContain('would hide');
    await key(go, 'Enter');
    expect(api.config.setKeybindingVariable).not.toHaveBeenCalled();
    await type(go, 'j');
    expect(root.getElementById('variable-check')!.textContent).toContain('3 keys change');
    await key(go, 'Enter');
    expect(api.config.setKeybindingVariable).toHaveBeenCalledWith('go', 'j');
    // The keybindings panel, maybe beside it, reads its lines again.
    expect(api.workspace.set).toHaveBeenCalledWith('keybindings.changed', expect.any(Number));
  });

  const names = (root: ShadowRoot) =>
    [...root.querySelectorAll('#variables tr code')].map((code) => code.textContent);

  test('the filter keeps the keys starting with what is typed, then the names holding it', async () => {
    const { root } = await mountPanel(() => '');
    const filter = root.getElementById('variable-filter') as HTMLInputElement;
    const filterBy = async (text: string) => {
      filter.value = text;
      filter.dispatchEvent(new Event('input'));
      await flush();
    };
    await filterBy('nel');
    expect(names(root)).toEqual(['{panels}']);
    await filterBy('{go}');
    expect(names(root)).toEqual(['{go}']);
    // The key `t` first, then the names holding it.
    await filterBy('t');
    expect(names(root)).toEqual(['{panels}', '{negative}', '{tags}']);
    await filterBy(', -');
    expect(names(root)).toEqual(['{negative}']);
    // However the chord is written.
    await filterBy('shift+ctrl');
    expect(names(root)).toEqual(['{tags}']);
    await filterBy('nothing');
    expect(names(root)).toEqual([]);
    expect(root.getElementById('variables-count')!.textContent).toBe('0 of 4');
    await filterBy('');
    expect(names(root)).toHaveLength(4);
    expect(root.getElementById('variables-count')!.textContent).toBe('4');
  });

  test('opening the panel selects no text field; keybindings-variables:focus filter does', async () => {
    const { root, handlers } = await mountPanel(() => '');
    const filter = root.getElementById('variable-filter') as HTMLInputElement;
    expect(root.activeElement).not.toBe(filter);
    await handlers.get('keybindings-variables:focus')!(undefined, 'filter');
    expect(root.activeElement).toBe(filter);
  });
});
