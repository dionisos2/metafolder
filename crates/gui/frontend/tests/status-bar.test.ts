// The question bar (StatusBar.svelte) shows a script's awaited keys, one
// keycap each. A script may name the same key twice (`mf gui input --keys
// y,y`): the bar must still show, not throw on a duplicate key.

import { afterEach, describe, expect, test, vi } from 'vitest';
import { flushSync, mount, unmount } from 'svelte';
import StatusBar from '../src/components/StatusBar.svelte';
import { questionsState, store } from '../src/lib/store.svelte';

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async () => null),
  listen: vi.fn(async () => () => {}),
}));

describe('the question bar', () => {
  let component: ReturnType<typeof mount> | null = null;

  afterEach(() => {
    if (component) void unmount(component);
    component = null;
    store.ui.questions = [];
    document.body.replaceChildren();
  });

  test('shows a question naming a key twice, each key once', () => {
    store.ui.questions = questionsState({
      questions: [{ id: 1, prompt: 'Keep it?', keys: ['y', 'n', 'y'] }],
    });
    const target = document.createElement('div');
    document.body.append(target);
    component = mount(StatusBar, { target });
    flushSync();
    const caps = [...document.querySelectorAll('.input-wait .keycap')].map((el) => el.textContent);
    expect(caps).toEqual(['y', 'n']);
  });
});

describe('the pending key hints', () => {
  let component: ReturnType<typeof mount> | null = null;

  afterEach(() => {
    if (component) void unmount(component);
    component = null;
    store.ui.pendingKeys = null;
    document.body.replaceChildren();
  });

  test('show a continuation bound in two scopes to the same command once', () => {
    // A global binding and a panel-local one, both eligible in that panel.
    const binding = (when: string | null) => ({
      keys: ['g', 'g'],
      invocation: 'cursor:first self',
      when,
      focus: null,
      text_input: false,
    });
    store.ui.pendingKeys = { prefix: ['g'], candidates: [binding(null), binding('file-manager')] };
    const target = document.createElement('div');
    document.body.append(target);
    component = mount(StatusBar, { target });
    flushSync();
    const hints = [...document.querySelectorAll('.key-hints .hint:not(.dim)')].map(
      (el) => el.textContent,
    );
    expect(hints).toEqual(['g cursor:first self']);
  });
});
