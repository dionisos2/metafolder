// Hints (doc "Hints"): every clickable element in view of the focused panel gets
// a label, and typing a label activates it. The tests give the elements their
// rectangles, since jsdom lays nothing out.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { closeHints, hintLabels, openHints, typeHint } from '../src/lib/hints';
import { setFindRootProvider } from '../src/lib/panels/roots';
import { store } from '../src/lib/store.svelte';

const { flashStatus } = vi.hoisted(() => ({ flashStatus: vi.fn() }));
vi.mock('../src/lib/store.svelte', async (original) => ({
  ...(await original<typeof import('../src/lib/store.svelte')>()),
  flashStatus,
}));

let host: HTMLElement;
let root: ShadowRoot;

const VIEW = { left: 0, top: 0, right: 500, bottom: 500 };

/** Mounts `html` as a panel. An element is 10 × 10 at `data-y` (0 by default). */
function panel(html: string) {
  root.innerHTML = html;
}

const labels = () =>
  store.ui.hints.items.filter((item) => item.label.startsWith(store.ui.hints.typed)).map((item) => item.label);

beforeEach(() => {
  document.body.innerHTML = '<div id="host"></div>';
  host = document.getElementById('host')!;
  root = host.attachShadow({ mode: 'open' });
  setFindRootProvider(() => root);
  vi.spyOn(Element.prototype, 'getBoundingClientRect').mockImplementation(function (this: Element) {
    if (this === host) return { ...VIEW, width: 500, height: 500 } as DOMRect;
    const top = Number((this as HTMLElement).dataset.y ?? 0);
    const size = (this as HTMLElement).hidden ? 0 : Number((this as HTMLElement).dataset.h ?? 10);
    return { left: 0, right: size, top, bottom: top + size, width: size, height: size } as DOMRect;
  });
});

afterEach(() => {
  closeHints();
  vi.restoreAllMocks();
  setFindRootProvider(null);
});

describe('hintLabels', () => {
  test('one letter while the alphabet suffices', () => {
    expect(hintLabels(3)).toEqual(['a', 's', 'd']);
  });

  test('two letters beyond, none the prefix of another', () => {
    const many = hintLabels(30);
    expect(new Set(many).size).toBe(30);
    expect(many.every((label) => label.length === 2)).toBe(true);
  });
});

describe('what is labelled', () => {
  test('links, buttons, text zones, checkboxes and selects, in document order', () => {
    panel(`<a id="1" href="#x">l</a><button id="2">b</button><input id="3" />
      <input id="4" type="checkbox" /><select id="5"></select><textarea id="6"></textarea>
      <div id="7" contenteditable="true"></div><p>text</p>`);
    openHints();
    expect(store.ui.hints.items.map((item) => item.label)).toEqual(['a', 's', 'd', 'f', 'g', 'h', 'j']);
  });

  test('an element made clickable by its style is labelled, its children are not', () => {
    panel(`<ul><li style="cursor: pointer"><span style="cursor: pointer">row</span></li></ul>`);
    openHints();
    expect(labels()).toEqual(['a']);
  });

  test('a control inside a clickable row is labelled as well', () => {
    panel(`<div style="cursor: pointer"><input type="checkbox" /></div>`);
    openHints();
    expect(labels()).toEqual(['a', 's']);
  });

  test('a label with its control counts once, the label', () => {
    panel(`<label style="cursor: pointer"><input type="checkbox" /> option</label>`);
    openHints();
    expect(labels()).toEqual(['a']);
    typeHint('a');
    expect(root.querySelector('input')?.checked).toBe(true);
  });

  test('what is disabled, hidden or out of view is not', () => {
    panel(`<button disabled>off</button><input type="hidden" /><button hidden>gone</button>
      <div hidden style="display: none"><button>inside</button></div>
      <button style="visibility: hidden">invisible</button>
      <button data-y="900">below</button><button>shown</button>`);
    openHints();
    expect(labels()).toEqual(['a']);
  });

  test('what its scrolling parent cuts off is not', () => {
    panel(`<div style="overflow: auto" data-h="50"><button data-y="20">in</button><button data-y="80">cut</button></div>`);
    openHints();
    expect(labels()).toEqual(['a']);
  });

  test('a panel with nothing to click says so and takes no keys', () => {
    panel('<p>text</p>');
    openHints();
    expect(store.ui.hints.open).toBe(false);
    expect(flashStatus).toHaveBeenCalledWith(expect.stringContaining('nothing'));
  });
});

describe('typing a label', () => {
  test('clicks a button and ends the hints', () => {
    panel('<button id="first">a</button><button id="second">b</button>');
    const clicked = vi.fn();
    root.getElementById('second')!.addEventListener('click', clicked);
    openHints();
    expect(typeHint('s')).toBe('');
    expect(clicked).toHaveBeenCalledOnce();
    expect(store.ui.hints.open).toBe(false);
  });

  test('toggles a checkbox', () => {
    panel('<input id="box" type="checkbox" />');
    openHints();
    typeHint('a');
    expect((root.getElementById('box') as HTMLInputElement).checked).toBe(true);
  });

  test.each(['<input id="t" />', '<textarea id="t"></textarea>', '<select id="t"></select>'])(
    'gives %s the focus without clicking',
    (html) => {
      panel(html);
      const clicked = vi.fn();
      root.getElementById('t')!.addEventListener('click', clicked);
      openHints();
      typeHint('a');
      expect(root.activeElement?.id).toBe('t');
      expect(clicked).not.toHaveBeenCalled();
    },
  );

  test('narrows two-letter labels, and refuses a letter none continues', () => {
    panel(Array.from({ length: 30 }, () => '<button>b</button>').join(''));
    openHints();
    expect(typeHint('a')).toBe('a');
    expect(labels().every((label) => label.startsWith('a'))).toBe(true);
    expect(store.ui.hints.open).toBe(true);
    expect(typeHint('A')).toBe('a');
    expect(typeHint('a1')).toBe('a');
  });

  test('a letter no label starts with is refused', () => {
    panel('<button>b</button>');
    openHints();
    expect(typeHint('z')).toBe('');
    expect(store.ui.hints.open).toBe(true);
  });

  test('an element the panel has redrawn meanwhile is not clicked, and it says so', () => {
    panel('<button id="old">b</button>');
    const clicked = vi.fn();
    root.getElementById('old')!.addEventListener('click', clicked);
    openHints();
    panel('<p>redrawn</p>');
    typeHint('a');
    expect(clicked).not.toHaveBeenCalled();
    expect(store.ui.hints.open).toBe(false);
    expect(flashStatus).toHaveBeenCalledWith(expect.stringContaining('gone'));
  });
});

test('closing leaves the panel untouched', () => {
  panel('<button>b</button>');
  openHints();
  expect(store.ui.hints.items.length).toBe(1);
  closeHints();
  expect(store.ui.hints.open).toBe(false);
  expect(store.ui.hints.items).toEqual([]);
  expect(root.querySelectorAll('button').length).toBe(1);
});
