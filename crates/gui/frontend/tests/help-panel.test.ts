// The help panel's keys (doc "help panel"): its search box is a text zone
// driven like the list's — `help:apply search [stay]`, `help:clear`,
// `help:focus search` — and `cursor:parent` goes up to the page's topic.

import { beforeEach, describe, expect, test, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { withRuns } from './support/runs';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/help');

const MANIFEST = [
  { id: 'documentation', title: 'Documentation', file: 'documentation.html' },
  { id: 'gui', title: 'GUI', file: 'gui.html', tags: ['Documentation'] },
  { id: 'help-panel', title: 'help panel', file: 'help-panel.html', tags: ['GUI'] },
  { id: 'links', title: 'links', file: 'links.html' },
  { id: 'many', title: 'many', file: 'many.html' },
];

// Pages with links, for the hints: `links` has two, `many` thirty.
const PAGES: Record<string, string> = {
  links: '<p>see <a data-help-page="gui">gui</a> and <a data-help-page="help-panel">help</a></p>',
  many: Array.from({ length: 30 }, () => '<a data-help-page="gui">gui</a>').join(' '),
};

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

async function mountHelp(request: unknown = null) {
  const root = shadow();
  const handlers = new Map<string, Handler>();
  const provided: Record<string, () => unknown> = {};
  const setVar = vi.fn(async () => {});
  const api = {
    guiServer: 'http://gui',
    whenVisible: (fn: () => void) => fn(),
    provide: (fns: Record<string, () => unknown>) => Object.assign(provided, fns),
    commands: {
      keybindings: async () => [],
      register: async (name: string, spec: { handler: Handler }) => {
        handlers.set(name, spec.handler);
      },
      invoke: () => null,
    },
    query: { grammarSource: async () => '' },
    statusBar: { message: async () => {} },
    workspace: { get: async () => request, set: setVar, onChange: () => {} },
  };
  const mod = await import('../../default-config/panel-types/help/main.js');
  await mod.mount(root, withRuns(api) as unknown as Metafolder.Api);
  const input = root.getElementById('help-search') as HTMLInputElement;
  const content = root.getElementById('help-content') as HTMLElement;
  const run = (name: string, ...args: string[]) => {
    const handler = handlers.get(name);
    if (!handler) throw new Error(`${name} is not registered`);
    return handler(...args);
  };
  /** Show a page as typing its name and Enter would. */
  const show = (name: string) => {
    input.value = name;
    return run('help:apply', 'search');
  };
  // jsdom does not scroll: give the scroll box a scrollTop that holds a value.
  const scroll = root.getElementById('help-scroll') as HTMLElement;
  let top = 0;
  Object.defineProperty(scroll, 'scrollTop', {
    get: () => top,
    set: (v: number) => {
      top = v;
    },
  });
  return { root, input, content, run, provided, show, scroll, setVar };
}

beforeEach(() => {
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: string) => {
      if (url.endsWith('/index.json')) return new Response(JSON.stringify(MANIFEST));
      const id = url.split('/').pop()?.replace('.html', '');
      return new Response(PAGES[id ?? ''] ?? `<p>page ${id}</p>`);
    }),
  );
});

describe('the search box is a text zone', () => {
  test('help:open asks for the focus once: the request is then marked as served', async () => {
    const request = { topic: '', nonce: 1, focus: true };
    const { root, input, setVar } = await mountHelp(request);
    await new Promise((r) => setTimeout(r, 0));
    expect(root.activeElement).toBe(input);
    expect(setVar).toHaveBeenCalledWith('help.request', { ...request, focus: false });
  });

  test('opening the panel, or asking it for a page, leaves the box unfocused', async () => {
    const { root } = await mountHelp();
    await new Promise((r) => setTimeout(r, 0));
    expect(root.activeElement).toBeNull();
  });

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

describe('history', () => {
  test('back and forward walk the pages shown, like a browser', async () => {
    const { input, content, run, show } = await mountHelp();
    await show('documentation');
    await show('gui');
    await show('help panel');
    await run('help:back');
    expect(content.textContent).toBe('page gui');
    expect(input.value).toBe('gui');
    await run('help:back');
    expect(content.textContent).toBe('page documentation');
    await run('help:back');
    expect(content.textContent).toBe('page documentation');
    await run('help:forward');
    expect(content.textContent).toBe('page gui');
    await run('help:forward');
    expect(content.textContent).toBe('page help-panel');
    await run('help:forward');
    expect(content.textContent).toBe('page help-panel');
  });

  test('opening a page after going back drops the pages ahead', async () => {
    const { content, run, show } = await mountHelp();
    await show('documentation');
    await show('gui');
    await run('help:back');
    await show('help panel');
    await run('help:forward');
    expect(content.textContent).toBe('page help-panel');
    await run('help:back');
    expect(content.textContent).toBe('page documentation');
  });

  test('back from a search goes to the page shown before it', async () => {
    const { content, run, show, input } = await mountHelp();
    await show('gui');
    await show('#page');
    expect(content.hidden).toBe(true);
    await run('help:back');
    expect(content.hidden).toBe(false);
    expect(content.textContent).toBe('page gui');
    expect(input.value).toBe('gui');
  });

  test('a page shown twice in a row is one step', async () => {
    const { content, run, show } = await mountHelp();
    await show('documentation');
    await show('gui');
    await show('gui');
    await run('help:back');
    expect(content.textContent).toBe('page documentation');
  });
});

describe('link hints', () => {
  /** The hint labels shown, in order. */
  const labels = (root: ShadowRoot) =>
    [...root.querySelectorAll<HTMLElement>('.link-hint')]
      .filter((hint) => !hint.hidden)
      .map((hint) => hint.textContent);
  const type = (field: HTMLInputElement, text: string) => {
    field.value = text;
    field.dispatchEvent(new Event('input'));
  };

  test('label every link with a letter and take the keys in a text zone', async () => {
    const { root, run, show } = await mountHelp();
    await show('links');
    await run('help:open-hints');
    expect(labels(root)).toEqual(['a', 's']);
    const field = root.activeElement as HTMLInputElement;
    expect(field.dataset.mfFocus).toBe('hints');
  });

  test("typing a link's letter opens its page and ends the hints", async () => {
    const { root, content, run, show } = await mountHelp();
    await show('links');
    await run('help:open-hints');
    type(root.activeElement as HTMLInputElement, 's');
    expect(content.textContent).toBe('page help-panel');
    expect(labels(root)).toEqual([]);
  });

  test('the topic buttons above a page are labelled too, first', async () => {
    const { root, content, run, show } = await mountHelp();
    await show('help panel');
    await run('help:open-hints');
    expect(labels(root)).toEqual(['a']);
    type(root.activeElement as HTMLInputElement, 'a');
    expect(content.textContent).toBe('page gui');
  });

  test('search results are labelled', async () => {
    const { root, content, run, show } = await mountHelp();
    await show('#page');
    await run('help:open-hints');
    expect(labels(root).length).toBe(3);
    type(root.activeElement as HTMLInputElement, labels(root)[0] ?? '');
    expect(content.hidden).toBe(false);
  });

  test('more links than letters take two letters each, narrowed as they are typed', async () => {
    const { root, content, run, show } = await mountHelp();
    await show('many');
    await run('help:open-hints');
    expect(labels(root).length).toBe(30);
    expect(labels(root).every((label) => label?.length === 2)).toBe(true);
    const field = root.activeElement as HTMLInputElement;
    type(field, 'a');
    expect(labels(root).every((label) => label?.startsWith('a'))).toBe(true);
    expect(content.textContent).not.toBe('page gui');
    type(field, 'as');
    expect(content.textContent).toBe('page gui');
  });

  test('a letter no label starts with is refused', async () => {
    const { root, run, show } = await mountHelp();
    await show('links');
    await run('help:open-hints');
    const field = root.activeElement as HTMLInputElement;
    type(field, 'z');
    expect(field.value).toBe('');
    expect(labels(root)).toEqual(['a', 's']);
  });

  test('leaving the zone (escape) cancels, the page untouched', async () => {
    const { root, content, run, show } = await mountHelp();
    await show('links');
    await run('help:open-hints');
    (root.activeElement as HTMLInputElement).blur();
    expect(labels(root)).toEqual([]);
    expect(content.querySelectorAll('a').length).toBe(2);
    expect(content.textContent).toBe('see gui and help');
  });

  test('a page without links says so and takes no keys', async () => {
    const { root, run, show } = await mountHelp();
    await show('documentation');
    await run('help:open-hints');
    expect(labels(root)).toEqual([]);
    expect(root.activeElement).toBeNull();
  });
});

describe('scroll position', () => {
  test('a new page opens at its top', async () => {
    const { show, scroll } = await mountHelp();
    await show('documentation');
    scroll.scrollTop = 500;
    await show('gui');
    expect(scroll.scrollTop).toBe(0);
  });

  test('back and forward return to where each page was left', async () => {
    const { run, show, scroll } = await mountHelp();
    await show('documentation');
    scroll.scrollTop = 300;
    await show('gui');
    scroll.scrollTop = 700;
    await run('help:back');
    expect(scroll.scrollTop).toBe(300);
    await run('help:forward');
    expect(scroll.scrollTop).toBe(700);
  });

  test('a search lists its results from the top', async () => {
    const { show, scroll } = await mountHelp();
    await show('documentation');
    scroll.scrollTop = 300;
    await show('#page');
    expect(scroll.scrollTop).toBe(0);
  });
});
