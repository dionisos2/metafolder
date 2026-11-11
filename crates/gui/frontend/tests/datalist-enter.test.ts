import { afterEach, describe, expect, it } from 'vitest';
import { completesOverDatalist, datalistTakesKey } from '../src/lib/datalistEnter';

/** A field completing over a datalist, inside a shadow root as panels are. */
function field(options: string[]): HTMLInputElement {
  const host = document.createElement('div');
  document.body.append(host);
  const root = host.attachShadow({ mode: 'open' });
  const input = document.createElement('input');
  input.setAttribute('list', 'choices');
  const list = document.createElement('datalist');
  list.id = 'choices';
  for (const value of options) {
    const option = document.createElement('option');
    option.value = value;
    list.append(option);
  }
  root.append(input, list);
  return input;
}

afterEach(() => {
  document.body.replaceChildren();
});

describe('completesOverDatalist', () => {
  it('is true for a field whose datalist offers something, in a shadow root', () => {
    expect(completesOverDatalist(field(['panel:open']))).toBe(true);
  });

  it('is false with an empty datalist, without one, or for anything else', () => {
    expect(completesOverDatalist(field([]))).toBe(false);
    expect(completesOverDatalist(document.createElement('input'))).toBe(false);
    expect(completesOverDatalist(document.createElement('textarea'))).toBe(false);
    expect(completesOverDatalist(null)).toBe(false);
  });
});

describe('datalistTakesKey', () => {
  it('leaves Enter to the keybindings while no suggestion is highlighted', () => {
    const input = field(['panel:open']);
    expect(datalistTakesKey(input, 'enter')).toBe(false);
  });

  it('gives Enter to the browser once an arrow has highlighted a suggestion', () => {
    const input = field(['panel:open']);
    expect(datalistTakesKey(input, 'down')).toBe(false); // the arrow itself passes
    expect(datalistTakesKey(input, 'enter')).toBe(true);
    // The pick closes the drop-down: the next Enter validates and leaves.
    expect(datalistTakesKey(input, 'enter')).toBe(false);
  });

  it('forgets the highlight on another key, or when the field is left', () => {
    const input = field(['panel:open']);
    datalistTakesKey(input, 'up');
    datalistTakesKey(input, 'p'); // typed: the suggestions are filtered anew
    expect(datalistTakesKey(input, 'enter')).toBe(false);

    datalistTakesKey(input, 'down');
    input.dispatchEvent(new FocusEvent('blur'));
    expect(datalistTakesKey(input, 'enter')).toBe(false);
  });

  it('never takes a key in a field without suggestions', () => {
    const input = field([]);
    datalistTakesKey(input, 'down');
    expect(datalistTakesKey(input, 'enter')).toBe(false);
  });
});
