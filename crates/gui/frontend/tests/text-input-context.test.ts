// inTextInputContext: keybindings with text-input=false must be suppressed
// whenever a form control holds focus — including a <select> (an open native
// select popup on WebKitGTK can dispatch keydowns whose composedPath()[0] is
// not the select, so the deep active element is consulted too).
import { describe, it, expect, afterEach } from 'vitest';
import { inTextInputContext, isTextInput } from '../src/lib/keys';

function add<T extends HTMLElement>(el: T): T {
  document.body.appendChild(el);
  return el;
}

afterEach(() => {
  document.body.replaceChildren();
});

describe('isTextInput', () => {
  it('covers input, textarea, select and contenteditable', () => {
    expect(isTextInput(document.createElement('input'))).toBe(true);
    expect(isTextInput(document.createElement('textarea'))).toBe(true);
    expect(isTextInput(document.createElement('select'))).toBe(true);
    expect(isTextInput(document.createElement('div'))).toBe(false);
    expect(isTextInput(null)).toBe(false);
  });
});

describe('inTextInputContext', () => {
  it('is true when the event target is a select', () => {
    const select = add(document.createElement('select'));
    expect(inTextInputContext(select)).toBe(true);
  });

  it('is true when a select holds focus even if the event target is not it', () => {
    const select = add(document.createElement('select'));
    select.focus();
    // Simulating the WebKitGTK quirk: the keydown target is the body.
    expect(inTextInputContext(document.body)).toBe(true);
  });

  it('is false when focus is on a non-input element', () => {
    const button = add(document.createElement('button'));
    button.focus();
    expect(inTextInputContext(button)).toBe(false);
  });
});
