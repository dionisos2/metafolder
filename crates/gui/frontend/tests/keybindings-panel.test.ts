// keybindings panel (doc "keybindings panel"): the editor of keybindings.toml.
// One row per line of the file — its key as written, variables left in and
// editable in place, beside the keys it really binds —, the commands no key
// runs, a filter by command class, and the variables. Typing in the form
// filters the rows, and saving goes through the backend's checks (doc
// "Changing keybindings"), which the form also shows while it is typed.

import { describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/keybindings');

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

type Entry = {
  source: string;
  command: string;
  when: string | null;
  focus: string | null;
  text_input: boolean;
  target: string | null;
  bindings: { keys: string[]; invocation: string }[];
  suggested: boolean;
  class: string | null;
};

const line = (source: string, command: string, extra: Partial<Entry> = {}): Entry => ({
  source,
  command,
  when: null,
  focus: null,
  text_input: false,
  target: null,
  bindings: [{ keys: source.split(' '), invocation: command }],
  suggested: false,
  class: 'B',
  ...extra,
});

const aimed = (keys: string, target: string) => ({
  keys: keys.split(' '),
  invocation: `panel:open ${target} metarecord-list`,
});

const ENTRIES: Entry[] = [
  line('x', 'command-input:focus command', { class: 'A' }),
  line('ctrl+g', 'find:open'),
  line('ctrl+shift+a', 'metarecord-list:select all', { when: 'metarecord-list' }),
  line('ctrl+t', 'workspace:new'),
  line('{go} g', 'cursor:first self', { bindings: [{ keys: ['g', 'g'], invocation: 'cursor:first self' }] }),
  line('{panels} l', 'panel:open {target} metarecord-list', {
    target: 'here',
    bindings: [aimed('t l', 'here'), aimed('i t l', 'here'), aimed('b t l', 'other'), aimed('n t l', 'new')],
  }),
  line('ctrl+alt+y', 'e2e:ping', { when: 'e2e', suggested: true, class: null }),
];

const VARIABLES = [
  { name: 'go', value: 'g', keys: 'g', uses: 1 },
  { name: 'panels', value: 't', keys: 't', uses: 1 },
  { name: 'negative', value: '{tags} -', keys: ', -', uses: 3 },
];

const UNBOUND = [
  { name: 'log:prune', label: 'Prune the log', class: 'X' },
  { name: 'reconcile:run', label: 'Reconcile the repository', class: 'D' },
];

const COMMANDS = [
  { name: 'workspace:new', label: 'New workspace', owner: null },
  { name: 'find:open', label: 'Find text', owner: null },
  { name: 'cursor:first', label: 'First row', owner: null },
  { name: 'panel:open', label: 'Open a panel', owner: null },
  { name: 'metarecord-list:select', label: 'Select', owner: 'metarecord-list' },
];

const flush = async () => {
  for (let i = 0; i < 10; i++) await new Promise((r) => setTimeout(r, 0));
};

type View = {
  entries: Entry[];
  variables: typeof VARIABLES;
  unbound: typeof UNBOUND;
  classes_error: string | null;
};

async function mountPanel(
  options: {
    check?: (draft: Record<string, unknown>) => Record<string, unknown>;
    checkVariable?: (name: string, value: string) => string;
    view?: Partial<View>;
    visible?: boolean;
  } = {},
) {
  let view: View = {
    entries: [...ENTRIES],
    variables: VARIABLES,
    unbound: UNBOUND,
    classes_error: null,
    ...options.view,
  };
  const api = {
    commands: {
      list: vi.fn(async () => COMMANDS),
    },
    config: {
      keybindingEntries: vi.fn(async () => view),
      checkKeybindingEntry: vi.fn(async (draft: Record<string, unknown>) => ({
        command: null,
        key: null,
        note: null,
        in_the_way: null,
        ...options.check?.(draft),
      })),
      saveKeybindingEntry: vi.fn(async () => view),
      removeKeybindingEntry: vi.fn(async (place: Record<string, unknown>) => {
        view = { ...view, entries: view.entries.filter((e) => e.source !== place.source) };
        return view;
      }),
      checkKeybindingVariable: vi.fn(async (name: string, value: string) =>
        options.checkVariable ? options.checkVariable(name, value) : '1 key changes.',
      ),
      setKeybindingVariable: vi.fn(async () => view),
    },
    visible: options.visible ?? false,
    onVisibility: vi.fn(),
    provide: vi.fn(),
  };
  const root = shadowRoot();
  const mod = await import('../../default-config/panel-types/keybindings/main.js');
  await mod.mount(root, api as never);
  await flush();
  const field = (id: string) => root.getElementById(id) as HTMLInputElement;
  const type = async (input: HTMLInputElement, text: string) => {
    input.value = text;
    input.dispatchEvent(new Event('input'));
    await flush();
  };
  const typeIn = (id: string, text: string) => type(field(id), text);
  const key = async (input: HTMLInputElement, name: string) => {
    input.dispatchEvent(new KeyboardEvent('keydown', { key: name }));
    await flush();
  };
  const rows = () =>
    [...root.querySelectorAll('#bindings tr')].map((tr) => ({
      source:
        tr.querySelector<HTMLInputElement>('input.source')?.value ??
        tr.querySelector('.source')?.textContent ??
        '',
      keys: tr.querySelector('.keys')?.textContent ?? '',
      text: tr.textContent ?? '',
      tr: tr as HTMLTableRowElement,
    }));
  const row = (source: string) => rows().find((r) => r.source === source)!;
  const shown = () => rows().map((r) => r.source);
  const button = (tr: Element, label: string) =>
    [...tr.querySelectorAll('button')].find((b) => b.textContent === label);
  const save = root.getElementById('save') as HTMLButtonElement;
  const classButton = (c: string) => root.getElementById(`class-${c}`) as HTMLButtonElement;
  return { api, root, field, type, typeIn, key, rows, row, shown, button, save, classButton };
}

describe('keybindings panel', () => {
  test('each line shows its key as written beside the keys it binds', async () => {
    const { row, button } = await mountPanel();
    const template = row('{panels} l');
    expect(template.keys).toContain('t l');
    expect(template.keys).toContain('i t l');
    expect(template.keys).toContain('n t l');
    expect(template.text).toContain('panel:open {target} metarecord-list');
    expect(row('{go} g').keys).toContain('g g');
    // The key as written is the editable one; a suggestion is not in the file.
    const suggested = row('ctrl+alt+y');
    expect(suggested.text).toContain('suggested');
    expect(button(suggested.tr, 'Remove')).toBeUndefined();
    expect((suggested.tr.querySelector('input.source') as HTMLInputElement).readOnly).toBe(true);
    expect(button(row('ctrl+t').tr, 'Remove')).toBeDefined();
  });

  test('the commands no key runs are listed too, after the lines', async () => {
    const { shown, row, rows, button } = await mountPanel();
    expect(shown().slice(-2)).toEqual(['', '']);
    const prune = rows().find((r) => r.text.includes('log:prune'))!;
    expect(prune.text).toContain('Prune the log');
    expect(button(prune.tr, 'Bind')).toBeDefined();
    expect(row('x').text).toContain('A');
  });

  test('a class switched off hides its commands', async () => {
    const { classButton, shown, rows } = await mountPanel();
    classButton('A').click();
    expect(classButton('A').getAttribute('aria-pressed')).toBe('false');
    expect(shown()).not.toContain('x');
    // D and ✕ alone: the commands with no key are what is left.
    for (const c of ['B', 'C', 'none']) classButton(c).click();
    const texts = rows().map((r) => r.text);
    expect(texts).toHaveLength(2);
    expect(texts.join(' ')).toContain('log:prune');
    classButton('X').click();
    expect(rows().map((r) => r.text).join(' ')).not.toContain('log:prune');
  });

  test('the key typed keeps the lines binding a key it starts, or written with it', async () => {
    const { typeIn, shown } = await mountPanel();
    await typeIn('source', 'i t');
    expect(shown()).toEqual(['{panels} l']);
    await typeIn('source', 'g');
    expect(shown()).toEqual(['{go} g']);
    await typeIn('source', '{go');
    expect(shown()).toEqual(['{go} g']);
    // Spelled the other way round, it is the same key.
    await typeIn('source', 'shift+ctrl+a');
    expect(shown()).toEqual(['ctrl+shift+a']);
  });

  test('the command typed keeps its lines and its commands with no key, its label shown', async () => {
    const { typeIn, shown, root, rows } = await mountPanel();
    await typeIn('command', 'workspace:');
    expect(shown()).toEqual(['ctrl+t']);
    await typeIn('command', 'workspace:new');
    expect(root.getElementById('command-check')!.textContent).toContain('New workspace');
    await typeIn('command', 'log:');
    expect(rows().map((r) => r.text).join(' ')).toContain('log:prune');
  });

  test('what saving would refuse is shown while typing, the line in the way marked', async () => {
    const { typeIn, root, save, api } = await mountPanel({
      check: (draft) =>
        draft.source === 'ctrl+t'
          ? {
              key: '`ctrl+t` is already bound globally to `workspace:new`',
              in_the_way: { keys: ['ctrl+t'], when: null, focus: null },
            }
          : draft.source === 'i o'
            ? { note: 'Overrides `i o`, which the line "o" binds.' }
            : {},
    });
    await typeIn('source', 'ctrl+t');
    await typeIn('command', 'find:open');
    expect(root.getElementById('key-check')!.textContent).toContain('already bound');
    expect(save.disabled).toBe(true);
    expect((root.querySelector('#bindings tr.conflict input.source') as HTMLInputElement).value).toBe(
      'ctrl+t',
    );

    await typeIn('source', 'i o');
    expect(save.disabled).toBe(false);
    expect(root.getElementById('key-check')!.textContent).toContain('Overrides');
    expect(api.config.checkKeybindingEntry).toHaveBeenLastCalledWith(
      {
        source: 'i o',
        command: 'find:open',
        when: null,
        focus: null,
        textInput: false,
        target: null,
      },
      null,
    );
  });

  test('Save writes the line and empties the form', async () => {
    const { typeIn, save, api, field } = await mountPanel();
    await typeIn('source', '{panels} k');
    await typeIn('command', 'panel:open {target} keybindings');
    await typeIn('target', 'here');
    await typeIn('when', 'log');
    save.click();
    await flush();
    expect(api.config.saveKeybindingEntry).toHaveBeenCalledWith(
      {
        source: '{panels} k',
        command: 'panel:open {target} keybindings',
        when: 'log',
        focus: null,
        textInput: false,
        target: 'here',
      },
      null,
    );
    expect(field('source').value).toBe('');
    expect(field('command').value).toBe('');
  });

  test('Enter pressed while the last key typed is still being checked saves once it is', async () => {
    const { field, api, typeIn } = await mountPanel();
    let release = () => {};
    await typeIn('source', 'alt+t');
    api.config.checkKeybindingEntry.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          release = () => resolve({ command: null, key: null, note: null, in_the_way: null });
        }),
    );
    field('command').value = 'workspace:new';
    field('command').dispatchEvent(new Event('input'));
    field('command').dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter' }));
    await flush();
    expect(api.config.saveKeybindingEntry).not.toHaveBeenCalled();
    release();
    await flush();
    expect(api.config.saveKeybindingEntry).toHaveBeenCalledWith(
      expect.objectContaining({ source: 'alt+t', command: 'workspace:new' }),
      null,
    );
  });

  test('Edit fills the form, and Save replaces the line edited', async () => {
    const { row, button, field, typeIn, save, api, root } = await mountPanel();
    button(row('{panels} l').tr, 'Edit')!.click();
    await flush();
    expect(field('source').value).toBe('{panels} l');
    expect(field('command').value).toBe('panel:open {target} metarecord-list');
    expect(field('target').value).toBe('here');
    expect(root.getElementById('editing')!.hidden).toBe(false);

    await typeIn('source', '{panels} j');
    save.click();
    await flush();
    expect(api.config.saveKeybindingEntry).toHaveBeenCalledWith(
      expect.objectContaining({ source: '{panels} j', target: 'here' }),
      { source: '{panels} l', when: null, focus: null },
    );
    expect(root.getElementById('editing')!.hidden).toBe(true);
  });

  test('the key of a line is edited in place: Enter saves it, Escape gives it back', async () => {
    const { row, type, key, api, root } = await mountPanel({
      check: (draft) => (draft.source === '{go} t' ? { key: '`g t` is already bound' } : {}),
    });
    const input = row('{go} g').tr.querySelector('input.source') as HTMLInputElement;
    await type(input, '{go} t');
    expect(input.classList.contains('invalid')).toBe(true);
    expect(root.getElementById('key-check')!.textContent).toContain('already bound');
    await key(input, 'Enter');
    expect(api.config.saveKeybindingEntry).not.toHaveBeenCalled();
    await key(input, 'Escape');
    expect(input.value).toBe('{go} g');

    await type(input, '{go} h');
    await key(input, 'Enter');
    expect(api.config.saveKeybindingEntry).toHaveBeenCalledWith(
      {
        source: '{go} h',
        command: 'cursor:first self',
        when: null,
        focus: null,
        textInput: false,
        target: null,
      },
      { source: '{go} g', when: null, focus: null },
    );
  });

  test('Remove removes the line', async () => {
    const { row, button, api, shown } = await mountPanel();
    button(row('{panels} l').tr, 'Remove')!.click();
    await flush();
    expect(api.config.removeKeybindingEntry).toHaveBeenCalledWith({
      source: '{panels} l',
      when: null,
      focus: null,
    });
    expect(shown()).not.toContain('{panels} l');
  });

  test('Bind puts a command with no key into the form', async () => {
    const { rows, button, field } = await mountPanel();
    const run = rows().find((r) => r.text.includes('reconcile:run'))!;
    button(run.tr, 'Bind')!.click();
    await flush();
    expect(field('command').value).toBe('reconcile:run');
  });

  test('each variable is listed with its keys; a new value is checked, then saved on Enter', async () => {
    const { root, type, key, api } = await mountPanel({
      checkVariable: (_name, value) => {
        if (value === 'x') throw new Error('`x` (`command-input:focus command`) would hide `x l`');
        return '3 keys change.';
      },
    });
    const variables = [...root.querySelectorAll('#variables tr')];
    expect(variables).toHaveLength(3);
    const negative = variables.find((tr) => tr.textContent.includes('negative'))!;
    const value = negative.querySelector('input')!;
    expect(value.value).toBe('{tags} -');
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
  });

  test('a command-classes.toml that cannot be read is said, the rest shown', async () => {
    const { root, shown } = await mountPanel({
      view: { classes_error: 'invalid command-classes.toml: "quit" must be one of A, B, C, D, X' },
    });
    expect(root.getElementById('error')!.textContent).toContain('command-classes.toml');
    expect(shown()).toContain('ctrl+t');
  });

  test('shown when it mounts, the key field takes the focus', async () => {
    const { root, field } = await mountPanel({ visible: true });
    expect(root.activeElement).toBe(field('source'));
  });
});
