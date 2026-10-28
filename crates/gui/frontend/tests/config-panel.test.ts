// config panel (doc "config panel"): the configuration files and the
// keybinding editor. Typing in the form filters the table — the key shows where
// it is bound already, the command shows its keys — and saving goes through the
// backend's checks (doc "Changing keybindings"), which the form also shows
// while it is typed.

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

type Binding = {
  keys: string[];
  invocation: string;
  when: string | null;
  text_input: boolean;
  focus: string | null;
  suggested: boolean;
};

const bind = (combo: string, invocation: string, extra: Partial<Binding> = {}): Binding => ({
  keys: combo.split(' '),
  invocation,
  when: null,
  text_input: false,
  focus: null,
  suggested: false,
  ...extra,
});

const TABLE: Binding[] = [
  bind('ctrl+g', 'find:open'),
  bind('ctrl+shift+a', 'metarecord-list:select all', { when: 'metarecord-list' }),
  bind('ctrl+t', 'workspace:new'),
  bind('g g', 'cursor:first self'),
  bind('g t', 'workspace:next'),
  bind('ctrl+alt+y', 'e2e:ping', { when: 'e2e', suggested: true }),
];

const COMMANDS = [
  { name: 'workspace:new', label: 'New workspace', owner: null },
  { name: 'workspace:next', label: 'Next workspace', owner: null },
  { name: 'find:open', label: 'Find text', owner: null },
  { name: 'cursor:first', label: 'First row', owner: null },
  { name: 'metarecord-list:select', label: 'Select', owner: 'metarecord-list' },
];

const file = (category: string, name: string, reload: string | null = null) => ({
  category,
  name,
  path: `/cfg/${name}`,
  reload,
});

const flush = async () => {
  for (let i = 0; i < 10; i++) await new Promise((r) => setTimeout(r, 0));
};

async function mountPanel(
  options: {
    check?: (draft: Record<string, unknown>) => { command: string | null; key: string | null };
    save?: (draft: Record<string, unknown>) => Binding[];
    visible?: boolean;
    repo?: string | null;
  } = {},
) {
  let table = [...TABLE];
  const vars: Record<string, unknown> = { active_repo: options.repo ?? null };
  const varListeners: Record<string, (() => void)[]> = {};
  const api = {
    commands: {
      keybindings: vi.fn(async () => table),
      list: vi.fn(async () => COMMANDS),
    },
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
      checkKeybinding: vi.fn(async (draft: Record<string, unknown>) =>
        options.check ? options.check(draft) : { command: null, key: null },
      ),
      setKeybinding: vi.fn(async (draft: Record<string, unknown>) => {
        if (options.save) table = options.save(draft);
        return table;
      }),
      removeKeybinding: vi.fn(async (place: Record<string, unknown>) => {
        table = table.filter((b) => b.keys.join(' ') !== place.combo);
        return table;
      }),
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
  const rows = () =>
    [...root.querySelectorAll('#bindings tr')].map((tr) => ({
      keys: tr.querySelector('.combo')?.textContent ?? '',
      text: tr.textContent ?? '',
      tr: tr as HTMLTableRowElement,
    }));
  const shown = () => rows().map((r) => r.keys);
  const button = (tr: Element, label: string) =>
    [...tr.querySelectorAll('button')].find((b) => b.textContent === label);
  const save = root.getElementById('save') as HTMLButtonElement;
  const fileRows = () =>
    [...root.querySelectorAll('#files tr')].map((tr) => tr.textContent ?? '');
  const changeVar = async (key: string, value: unknown) => {
    vars[key] = value;
    for (const listener of varListeners[key] ?? []) listener();
    await flush();
  };
  return { api, root, field, type, rows, shown, button, save, fileRows, changeVar, vars };
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

  test('shown when it mounts, the key field takes the focus', async () => {
    // `panel:open here config` in a workspace that never showed the panel builds it on
    // screen: it is visible before its mount has run, so no visibility change
    // will follow.
    const { root, field } = await mountPanel({ visible: true });
    expect(root.activeElement).toBe(field('combo'));
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

  test('every binding is listed, a suggested one without Remove', async () => {
    const { shown, rows, button } = await mountPanel();
    expect(shown()).toHaveLength(TABLE.length);
    const suggested = rows().find((r) => r.keys === 'ctrl+alt+y')!;
    expect(suggested.text).toContain('suggested');
    expect(button(suggested.tr, 'Remove')).toBeUndefined();
    expect(button(rows().find((r) => r.keys === 'ctrl+t')!.tr, 'Remove')).toBeDefined();
  });

  test('the key typed filters the table to the keys it starts', async () => {
    const { type, shown } = await mountPanel();
    await type('combo', 'g');
    expect(shown()).toEqual(['g g', 'g t']);
    await type('combo', 'g g');
    expect(shown()).toEqual(['g g']);
    // Spelled the other way round, it is the same key.
    await type('combo', 'shift+ctrl+a');
    expect(shown()).toEqual(['ctrl+shift+a']);
    // A modifier being typed already narrows.
    await type('combo', 'ctrl');
    expect(shown()).toEqual(['ctrl+g', 'ctrl+shift+a', 'ctrl+t', 'ctrl+alt+y']);
  });

  test('the command typed filters the table to its keys, its label shown', async () => {
    const { type, shown, root } = await mountPanel();
    await type('command', 'workspace:');
    expect(shown()).toEqual(['ctrl+t', 'g t']);
    await type('command', 'workspace:new');
    expect(root.getElementById('command-check')!.textContent).toContain('New workspace');
  });

  test('a key and a command typed together show both: where the key is taken stays visible', async () => {
    const { type, shown } = await mountPanel();
    await type('combo', 'ctrl+t');
    await type('command', 'find:open');
    expect(shown()).toEqual(['ctrl+t', 'ctrl+g']);
  });

  test('what saving would refuse is shown while typing, and Save waits for it to be fixed', async () => {
    const { type, root, save, api } = await mountPanel({
      check: (draft) => ({
        command: draft.command === 'nope' ? 'unknown command `nope`' : null,
        key: draft.combo === 'ctrl+t' ? '`ctrl+t` is already bound globally to `workspace:new`' : null,
      }),
    });
    await type('combo', 'ctrl+t');
    await type('command', 'nope');
    expect(root.getElementById('command-check')!.textContent).toContain('unknown command');
    expect(root.getElementById('key-check')!.textContent).toContain('already bound');
    expect(save.disabled).toBe(true);
    // The row in the way is marked.
    expect(root.querySelector('#bindings tr.conflict .combo')?.textContent).toBe('ctrl+t');

    await type('combo', 'alt+t');
    await type('command', 'workspace:new');
    expect(save.disabled).toBe(false);
    expect(api.config.checkKeybinding).toHaveBeenLastCalledWith({
      combo: 'alt+t',
      command: 'workspace:new',
      when: null,
      focus: null,
      replacing: null,
    });
  });

  test('Save binds the key, empties the form and shows the new table', async () => {
    const { type, save, api, field, shown } = await mountPanel({
      save: () => [...TABLE, bind('alt+t', 'workspace:new', { when: 'log' })],
    });
    await type('combo', 'alt+t');
    await type('command', 'workspace:new');
    await type('when', 'log');
    save.click();
    await flush();
    expect(api.config.setKeybinding).toHaveBeenCalledWith({
      combo: 'alt+t',
      command: 'workspace:new',
      when: 'log',
      focus: null,
      textInput: false,
      replacing: null,
    });
    expect(field('combo').value).toBe('');
    expect(field('command').value).toBe('');
    expect(shown()).toContain('alt+t');
  });

  test('Enter in a field saves', async () => {
    const { type, api, field } = await mountPanel();
    await type('combo', 'alt+t');
    await type('command', 'workspace:new');
    field('command').dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter' }));
    await flush();
    expect(api.config.setKeybinding).toHaveBeenCalled();
  });

  test('Edit fills the form, and Save replaces the binding edited', async () => {
    const { rows, button, field, type, save, api, root } = await mountPanel();
    button(rows().find((r) => r.keys === 'ctrl+shift+a')!.tr, 'Edit')!.click();
    await flush();
    expect(field('combo').value).toBe('ctrl+shift+a');
    expect(field('command').value).toBe('metarecord-list:select all');
    expect(field('when').value).toBe('metarecord-list');
    expect(root.getElementById('editing')!.hidden).toBe(false);

    await type('combo', 'ctrl+shift+s');
    save.click();
    await flush();
    expect(api.config.setKeybinding).toHaveBeenCalledWith(
      expect.objectContaining({
        combo: 'ctrl+shift+s',
        replacing: { combo: 'ctrl+shift+a', when: 'metarecord-list', focus: null },
      }),
    );
    expect(root.getElementById('editing')!.hidden).toBe(true);
  });

  test('Cancel leaves the edit without saving', async () => {
    const { rows, button, field, root, api } = await mountPanel();
    button(rows().find((r) => r.keys === 'ctrl+t')!.tr, 'Edit')!.click();
    await flush();
    (root.getElementById('cancel') as HTMLButtonElement).click();
    await flush();
    expect(field('combo').value).toBe('');
    expect(root.getElementById('editing')!.hidden).toBe(true);
    expect(api.config.setKeybinding).not.toHaveBeenCalled();
  });

  test('a refusal of the backend is shown', async () => {
    const { type, save, api, root } = await mountPanel();
    api.config.setKeybinding.mockRejectedValueOnce('cannot write keybindings.toml');
    await type('combo', 'alt+t');
    await type('command', 'workspace:new');
    save.click();
    await flush();
    expect(root.getElementById('error')!.textContent).toContain('cannot write');
  });

  test('Remove unbinds the key', async () => {
    const { rows, button, api, shown } = await mountPanel();
    button(rows().find((r) => r.keys === 'g t')!.tr, 'Remove')!.click();
    await flush();
    expect(api.config.removeKeybinding).toHaveBeenCalledWith({ combo: 'g t', when: null, focus: null });
    expect(shown()).not.toContain('g t');
  });
});
