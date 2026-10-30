// config panel (doc "config panel"): the configuration files, filtered as a
// path is typed, each shown in the viewer or read again. The keybinding editor
// is the keybindings panel's (keybindings-panel.test.ts).

import { describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/config');

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

const file = (category: string, name: string, reload: string | null = null) => ({
  category,
  name,
  path: `/cfg/${name}`,
  reload,
});

const flush = async () => {
  for (let i = 0; i < 10; i++) await new Promise((r) => setTimeout(r, 0));
};

async function mountPanel(options: { visible?: boolean; repo?: string | null } = {}) {
  const vars: Record<string, unknown> = { active_repo: options.repo ?? null };
  const varListeners: Record<string, (() => void)[]> = {};
  const handlers = new Map<string, (...args: unknown[]) => unknown>();
  const api = {
    config: {
      info: vi.fn(async (repo?: string | null) => ({
        root: '/cfg',
        repo_config: repo ? '/music/.metafolder/config' : null,
        files: [
          file('cli', 'cli/config.toml'),
          file('core', 'core/query-grammar', 'grammar'),
          file('gui', 'gui/commands.js', 'commands'),
          file('gui', 'gui/keybindings.toml', 'keybindings'),
          file('gui', 'gui/macros.js', 'commands'),
          file('gui', 'gui/style.css', 'style'),
          file('gui', 'gui/user-commands.js', 'commands'),
          file('panel-types', 'gui/panel-types/viewer/main.js'),
          ...(repo
            ? [
                {
                  category: 'repository',
                  name: 'schema.json',
                  path: '/music/.metafolder/config/schema.json',
                  reload: null,
                },
              ]
            : []),
        ],
      })),
    },
    workspace: {
      get: vi.fn(async (key: string) => vars[key] ?? null),
      set: vi.fn(async (key: string, value: unknown) => {
        vars[key] = value;
      }),
      onChange: vi.fn((key: string, listener: () => void) => {
        (varListeners[key] ??= []).push(listener);
        return () => {};
      }),
    },
    commands: {
      register: vi.fn(async (name: string, spec: { handler: (...args: unknown[]) => unknown }) => {
        handlers.set(name, spec.handler);
      }),
    },
    invoke: vi.fn(async () => {}),
    visible: options.visible ?? false,
    onVisibility: vi.fn(),
    provide: vi.fn(),
  };
  const root = shadowRoot();
  const mod = await import('../../default-config/panel-types/config/main.js');
  await mod.mount(root, api as never);
  await flush();
  const field = (id: string) => root.getElementById(id) as HTMLInputElement;
  const type = async (id: string, text: string) => {
    field(id).value = text;
    field(id).dispatchEvent(new Event('input'));
    await flush();
  };
  const fileRows = () =>
    [...root.querySelectorAll('#files tr')].map((tr) => tr.textContent ?? '');
  const changeVar = async (key: string, value: unknown) => {
    vars[key] = value;
    for (const listener of varListeners[key] ?? []) listener();
    await flush();
  };
  return { api, handlers, root, field, type, fileRows, changeVar, vars };
}

describe('config panel', () => {
  test('it lists every configuration file, under its category', async () => {
    const { root } = await mountPanel();
    const text = root.getElementById('files')!.textContent;
    expect(text).toContain('/cfg/gui/keybindings.toml');
    expect(text).toContain('/cfg/cli/config.toml');
    expect(text).toContain('/cfg/gui/panel-types/viewer/main.js');
    const headings = [...root.querySelectorAll('#files th')].map((th) => th.textContent);
    expect(headings).toEqual(['cli', 'core', 'gui', 'panel-types']);
  });

  test("the workspace's repository has a category of its own", async () => {
    const { root, api, changeVar } = await mountPanel({ repo: 'abc' });
    expect(api.config.info).toHaveBeenLastCalledWith('abc');
    const headings = () => [...root.querySelectorAll('#files th')].map((th) => th.textContent);
    expect(headings()).toContain('repository');
    expect(root.getElementById('files')!.textContent).toContain(
      '/music/.metafolder/config/schema.json',
    );
    // Another repository in the workspace: the list follows it.
    await changeVar('active_repo', null);
    expect(api.config.info).toHaveBeenLastCalledWith(null);
    expect(headings()).not.toContain('repository');
  });

  test('the filter narrows the files to those whose path contains it', async () => {
    const { type, fileRows, root } = await mountPanel();
    await type('file-filter', 'TOML');
    const rows = fileRows().filter((text) => text.includes('/cfg/'));
    expect(rows).toHaveLength(2);
    expect(rows.join(' ')).toContain('cli/config.toml');
    expect(rows.join(' ')).toContain('gui/keybindings.toml');
    // A category left empty is not shown.
    const headings = [...root.querySelectorAll('#files th')].map((th) => th.textContent);
    expect(headings).toEqual(['cli', 'gui']);
    expect(root.getElementById('files-count')!.textContent).toBe('2 of 8 files');
  });

  test('Show selects the file and opens it in the viewer panel beside', async () => {
    const { root, api, vars } = await mountPanel();
    const row = [...root.querySelectorAll('#files tr')].find((tr) =>
      tr.textContent.includes('cli/config.toml'),
    )!;
    [...row.querySelectorAll('button')].find((b) => b.textContent === 'Show')!.click();
    await flush();
    expect(vars.selected_metarecord).toBeNull();
    expect(vars.selected_paths).toEqual(['/cfg/cli/config.toml']);
    expect(api.invoke).toHaveBeenCalledWith(['panel:open', 'other', 'viewer']);
  });

  test('opening the panel selects no text field; config:focus filter does', async () => {
    // `panel:open here config` in a workspace that never showed the panel builds it on
    // screen: it is visible before its mount has run.
    const { root, field, handlers } = await mountPanel({ visible: true });
    expect(root.activeElement).toBeNull();
    await handlers.get('config:focus')!(undefined, 'filter');
    expect(root.activeElement).toBe(field('file-filter'));
  });

  test('every reload target has its button, and all of them one more', async () => {
    const { root, api } = await mountPanel();
    const reloads = [
      ...root.querySelectorAll<HTMLButtonElement>('#files button.reload, #reload-all'),
    ];
    for (const button of reloads) button.click();
    await flush();
    const targets = api.invoke.mock.calls.map((call) => (call as unknown[])[0]);
    expect(new Set(targets.map((t) => JSON.stringify(t)))).toEqual(
      new Set(
        ['keybindings', 'style', 'commands', 'grammar', 'all'].map((what) =>
          JSON.stringify(['config:reload', what]),
        ),
      ),
    );
    // One per file it reloads — the three command files have one each — and
    // Reload all.
    expect(targets).toHaveLength(7);
  });
});
