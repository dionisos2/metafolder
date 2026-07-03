// Input-history helper (panel-shim/history.js): readline-style ctrl-p/ctrl-n
// navigation + ctrl-r OSM search overlay, shared by the shell command input
// and the panel text zones (spec-gui "Input history").

import { afterEach, describe, expect, test, vi } from 'vitest';
// @ts-expect-error plain-JS module shared with the panels
import { attachHistory, filterHistory } from '/__history.js';

type Attached = { push: (text: string) => void; detach: () => void };

const REPO = 'abc123';

function makeInput(container: HTMLElement = document.body): HTMLInputElement {
  const input = document.createElement('input');
  container.appendChild(input);
  return input;
}

function press(el: HTMLElement, key: string, init: KeyboardEventInit = {}) {
  const event = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true, ...init });
  el.dispatchEvent(event);
  return event;
}

function type(input: HTMLInputElement, value: string) {
  input.value = value;
  input.dispatchEvent(new Event('input', { bubbles: true }));
}

/** Flushes the microtask/timeout queue so async key handling settles. */
async function flush() {
  await new Promise((resolve) => setTimeout(resolve, 0));
}

/** A fake history store (the Tauri `history_read`/`history_append` pair). */
function fakeStore(entries: string[]) {
  const appends: Array<{ repo: string; zone: string; entry: string }> = [];
  const read = vi.fn(async (_repo: string, _zone: string) => entries);
  const append = vi.fn(async (repo: string, zone: string, entry: string) => {
    appends.push({ repo, zone, entry });
    return true;
  });
  return { read, append, appends };
}

function attach(
  input: HTMLInputElement,
  overrides: Record<string, unknown> = {},
  entries: string[] = ['one', 'two', 'three'],
) {
  const store = fakeStore(entries);
  const attached: Attached = attachHistory(input, {
    zone: 'shell:command',
    read: store.read,
    append: store.append,
    getRepo: async () => REPO,
    container: document.body,
    ...overrides,
  });
  return { attached, store };
}

afterEach(() => {
  document.body.replaceChildren();
});

describe('filterHistory', () => {
  test('returns newest first, deduped, empty filter keeps all', () => {
    expect(filterHistory(['a', 'b', 'a', 'c'], '')).toEqual(['c', 'a', 'b']);
  });

  test('filters with OSM semantics', () => {
    expect(filterHistory(['repo:list', 'metarecord get', 'repo:init'], 'rep in')).toEqual([
      'repo:init',
    ]);
    expect(filterHistory(['repo:list', 'metarecord get', 'repo:init'], 'record')).toEqual([
      'metarecord get',
    ]);
  });
});

describe('ctrl-p / ctrl-n navigation', () => {
  test('ctrl-p recalls entries newest first and stops at the oldest', async () => {
    const input = makeInput();
    attach(input);
    press(input, 'p', { ctrlKey: true });
    await flush();
    expect(input.value).toBe('three');
    press(input, 'p', { ctrlKey: true });
    await flush();
    expect(input.value).toBe('two');
    press(input, 'p', { ctrlKey: true });
    await flush();
    expect(input.value).toBe('one');
    press(input, 'p', { ctrlKey: true }); // already at the oldest
    await flush();
    expect(input.value).toBe('one');
  });

  test('ctrl-n walks forward and restores the in-progress draft', async () => {
    const input = makeInput();
    attach(input);
    type(input, 'my draft');
    press(input, 'p', { ctrlKey: true });
    await flush();
    press(input, 'p', { ctrlKey: true });
    await flush();
    expect(input.value).toBe('two');
    press(input, 'n', { ctrlKey: true });
    await flush();
    expect(input.value).toBe('three');
    press(input, 'n', { ctrlKey: true });
    await flush();
    expect(input.value).toBe('my draft');
  });

  test('recall dispatches a bubbling input event (bind:value compatibility)', async () => {
    const input = makeInput();
    attach(input);
    const seen: string[] = [];
    document.body.addEventListener('input', () => seen.push(input.value));
    press(input, 'p', { ctrlKey: true });
    await flush();
    expect(seen).toEqual(['three']);
    // ...and that self-dispatched event did not reset the navigation.
    press(input, 'p', { ctrlKey: true });
    await flush();
    expect(input.value).toBe('two');
  });

  test('typing resets navigation; the next ctrl-p starts from the edited text', async () => {
    const input = makeInput();
    attach(input);
    press(input, 'p', { ctrlKey: true });
    await flush();
    expect(input.value).toBe('three');
    type(input, 'edited');
    press(input, 'p', { ctrlKey: true });
    await flush();
    expect(input.value).toBe('three');
    press(input, 'n', { ctrlKey: true });
    await flush();
    expect(input.value).toBe('edited'); // the new draft is the edited text
  });

  test('a disabled zone (function returning null) leaves keys untouched', async () => {
    const input = makeInput();
    const { store } = attach(input, { zone: () => null });
    type(input, 'draft');
    const event = press(input, 'p', { ctrlKey: true });
    await flush();
    expect(input.value).toBe('draft');
    expect(event.defaultPrevented).toBe(false);
    expect(store.read).not.toHaveBeenCalled();
  });

  test('the zone function is evaluated at keypress time', async () => {
    const input = makeInput();
    let zone = 'shell:command';
    const { store } = attach(input, { zone: () => zone });
    press(input, 'p', { ctrlKey: true });
    await flush();
    expect(store.read).toHaveBeenLastCalledWith(REPO, 'shell:command');
    type(input, ''); // reset navigation
    zone = 'shell:bash';
    press(input, 'p', { ctrlKey: true });
    await flush();
    expect(store.read).toHaveBeenLastCalledWith(REPO, 'shell:bash');
  });
});

describe('push', () => {
  test('appends the entry to the zone history', async () => {
    const input = makeInput();
    const { attached, store } = attach(input);
    attached.push('repo:list');
    await flush();
    expect(store.appends).toEqual([{ repo: REPO, zone: 'shell:command', entry: 'repo:list' }]);
  });

  test('skips blank entries', async () => {
    const input = makeInput();
    const { attached, store } = attach(input);
    attached.push('   ');
    await flush();
    expect(store.appends).toEqual([]);
  });

  test('without a repo, entries stay in session memory and are recallable', async () => {
    const input = makeInput();
    const { attached, store } = attach(input, { getRepo: async () => null });
    attached.push('local-only');
    attached.push('local-only'); // consecutive dedup
    attached.push('second');
    await flush();
    expect(store.appends).toEqual([]);
    press(input, 'p', { ctrlKey: true });
    await flush();
    expect(input.value).toBe('second');
    press(input, 'p', { ctrlKey: true });
    await flush();
    expect(input.value).toBe('local-only');
    press(input, 'p', { ctrlKey: true });
    await flush();
    expect(input.value).toBe('local-only'); // deduped: only two entries
  });
});

describe('ctrl-r search overlay', () => {
  function overlay(): HTMLElement | null {
    return document.body.querySelector('.mf-history-overlay');
  }
  function searchInput(): HTMLInputElement {
    const el = overlay()?.querySelector<HTMLInputElement>('input');
    if (!el) throw new Error('no search input');
    return el;
  }
  function items(): string[] {
    return [...(overlay()?.querySelectorAll('.mf-history-item') ?? [])].map(
      (el) => el.textContent ?? '',
    );
  }

  test('opens listing entries newest first and filters with OSM', async () => {
    const input = makeInput();
    attach(input, {}, ['repo:list', 'metarecord get', 'repo:init']);
    press(input, 'r', { ctrlKey: true });
    await flush();
    expect(overlay()).not.toBeNull();
    expect(items()).toEqual(['repo:init', 'metarecord get', 'repo:list']);
    type(searchInput(), 'repo');
    await flush();
    expect(items()).toEqual(['repo:init', 'repo:list']);
  });

  test('Enter inserts the selection into the input and closes', async () => {
    const input = makeInput();
    attach(input, {}, ['repo:list', 'metarecord get']);
    press(input, 'r', { ctrlKey: true });
    await flush();
    // Selection starts on the newest; ctrl-n moves to the older entry.
    press(searchInput(), 'n', { ctrlKey: true });
    press(searchInput(), 'Enter');
    await flush();
    expect(overlay()).toBeNull();
    expect(input.value).toBe('repo:list');
  });

  test('Escape closes without inserting', async () => {
    const input = makeInput();
    attach(input);
    type(input, 'draft');
    press(input, 'r', { ctrlKey: true });
    await flush();
    press(searchInput(), 'Escape');
    await flush();
    expect(overlay()).toBeNull();
    expect(input.value).toBe('draft');
  });

  test('focusout closes without inserting (the shell Escape path blurs)', async () => {
    const input = makeInput();
    attach(input);
    press(input, 'r', { ctrlKey: true });
    await flush();
    searchInput().dispatchEvent(new FocusEvent('focusout', { bubbles: true }));
    await flush();
    expect(overlay()).toBeNull();
  });

  test('detach removes listeners and any open overlay', async () => {
    const input = makeInput();
    const { attached } = attach(input);
    press(input, 'r', { ctrlKey: true });
    await flush();
    attached.detach();
    expect(overlay()).toBeNull();
    press(input, 'p', { ctrlKey: true });
    await flush();
    expect(input.value).toBe('');
  });
});

describe('shadow DOM', () => {
  test('overlay and style land inside the shadow root', async () => {
    const host = document.createElement('div');
    document.body.appendChild(host);
    const shadow = host.attachShadow({ mode: 'open' });
    const body = document.createElement('div');
    shadow.appendChild(body);
    const input = makeInput(body);
    const store = fakeStore(['x']);
    attachHistory(input, {
      zone: 'metarecord-list:finder',
      read: store.read,
      append: store.append,
      getRepo: async () => REPO,
      container: body,
    });
    press(input, 'r', { ctrlKey: true });
    await flush();
    expect(shadow.querySelector('.mf-history-overlay')).not.toBeNull();
    expect(shadow.querySelector('style#mf-history-style')).not.toBeNull();
    expect(document.body.querySelector('.mf-history-overlay')).toBeNull();
  });
});
