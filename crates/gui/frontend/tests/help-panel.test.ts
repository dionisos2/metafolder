// The help panel's keys (doc "help panel"): its search box is a text zone
// driven like the list's — `help:apply search [stay]`, `help:clear`,
// `help:focus search` — and `cursor:parent` goes up to the page's topic.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/help');

const MANIFEST = [
  { id: 'documentation', title: 'Documentation', file: 'documentation.html' },
  { id: 'gui', title: 'GUI', file: 'gui.html', tags: ['Documentation'] },
  { id: 'help-panel', title: 'help panel', file: 'help-panel.html', tags: ['GUI'] },
];

function shadow(): ShadowRoot {
  const html = readFileSync(resolve(PANEL_DIR, 'index.html'), 'utf8');
  const doc = new DOMParser().parseFromString(html, 'text/html');
  const host = document.createElement('div');
  document.body.append(host);
  const root = host.attachShadow({ mode: 'open' });
  const body = document.createElement('div');
  body.className = 'mf-panel-body';
  for (const child of [...doc.body.childNodes]) {
    if (child.nodeName === 'SCRIPT' || child.nodeName === 'STYLE') continue;
    body.append(child);
  }
  root.append(body);
  return root;
}

type Handler = (...args: string[]) => unknown;

async function mountHelp() {
  const root = shadow();
  const handlers = new Map<string, Handler>();
  const provided: Record<string, () => unknown> = {};
  const api = {
    guiServer: 'http://gui',
    whenVisible: () => {},
    provide: (fns: Record<string, () => unknown>) => Object.assign(provided, fns),
    commands: {
      keybindings: async () => [],
      register: async (name: string, spec: { handler: Handler }) => {
        handlers.set(name, spec.handler);
      },
      invoke: () => null,
    },
    query: { grammarSource: async () => '' },
    workspace: { get: async () => null, onChange: () => {} },
  };
  const mod = await import('../../default-config/panel-types/help/main.js');
  await mod.mount(root, api as unknown as Metafolder.Api);
  const input = root.getElementById('help-search') as HTMLInputElement;
  const content = root.getElementById('help-content') as HTMLElement;
  const run = (name: string, ...args: string[]) => {
    const handler = handlers.get(name);
    if (!handler) throw new Error(`${name} is not registered`);
    return handler(...args);
  };
  return { root, input, content, run, provided };
}

beforeEach(() => {
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: string) => {
      if (url.endsWith('/index.json')) return new Response(JSON.stringify(MANIFEST));
      const id = url.split('/').pop()?.replace('.html', '');
      return new Response(`<p>page ${id}</p>`);
    }),
  );
});

describe('the search box is a text zone', () => {
  test('the box is tagged as the "search" focus scope', async () => {
    const { input } = await mountHelp();
    expect(input.dataset.mfFocus).toBe('search');
  });

  test('apply search runs the search and leaves the box; stay keeps the focus', async () => {
    const { root, input, content, run } = await mountHelp();
    input.focus();
    input.value = 'gui';
    await run('help:apply', 'search');
    expect(content.textContent).toBe('page gui');
    expect(root.activeElement).toBeNull();

    input.focus();
    input.value = 'help panel';
    await run('help:apply', 'search', 'stay');
    expect(content.textContent).toBe('page help-panel');
    expect(root.activeElement).toBe(input);
  });

  test('focus search gives the box the focus and keeps its text', async () => {
    const { root, input, run } = await mountHelp();
    input.value = 'gui';
    await run('help:focus', 'search');
    expect(root.activeElement).toBe(input);
    expect(input.value).toBe('gui');
  });

  test('clear search empties the box and gives it the focus', async () => {
    const { root, input, run } = await mountHelp();
    input.value = 'gui';
    await run('help:clear', 'search');
    expect(input.value).toBe('');
    expect(root.activeElement).toBe(input);
  });

  test('clear search stay empties the box and leaves the focus where it is', async () => {
    const { root, input, run } = await mountHelp();
    input.value = 'gui';
    input.blur();
    await run('help:clear', 'search', 'stay');
    expect(input.value).toBe('');
    expect(root.activeElement).toBeNull();
  });
});

describe('cursor:parent', () => {
  test("goes up to the page of the current page's first tag", async () => {
    const { input, content, run, provided } = await mountHelp();
    input.value = 'help panel';
    await run('help:apply', 'search');
    await provided.parent();
    expect(content.textContent).toBe('page gui');
    expect(input.value).toBe('gui');
    await provided.parent();
    expect(content.textContent).toBe('page documentation');
  });

  test('does nothing on the entry page', async () => {
    const { input, content, run, provided } = await mountHelp();
    input.value = 'documentation';
    await run('help:apply', 'search');
    await provided.parent();
    expect(content.textContent).toBe('page documentation');
  });
});
