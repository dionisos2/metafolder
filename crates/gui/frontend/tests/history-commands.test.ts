// The history:* commands (doc "Input history"): which field an argument
// designates, whether a key bound to them fires, and what they do to it.

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { afterEach, describe, expect, test, vi } from 'vitest';
import { attachHistory } from '/__history.js';
import {
  historyApplies,
  historyFieldNames,
  resolveHistoryField,
  runHistoryCommand,
} from '../src/lib/historyCommands';

const flush = () => new Promise((done) => setTimeout(done, 0));

/** A panel-like root holding a `finder` with a history and a `filter` without. */
function makeRoot(zone: string | (() => string | null) = 'metarecord-list:finder') {
  const root = document.createElement('div');
  document.body.appendChild(root);
  const finder = document.createElement('input');
  finder.dataset.mfFocus = 'finder';
  const filter = document.createElement('input');
  filter.dataset.mfFocus = 'filter';
  root.append(finder, filter);
  const read = vi.fn(async () => ['one', 'two']);
  attachHistory(finder, {
    zone,
    read,
    append: async () => true,
    getRepo: async () => 'repo',
    container: root,
  });
  return { root, finder, filter, read };
}

afterEach(() => {
  document.body.replaceChildren();
});

describe('resolveHistoryField', () => {
  test('self-focus is the focused field, when it has a history', () => {
    const { finder, filter, root } = makeRoot();
    expect(resolveHistoryField('self-focus', finder, root)).toBe(finder);
    expect(resolveHistoryField('self-focus', filter, root)).toBeNull();
    expect(resolveHistoryField('self-focus', null, root)).toBeNull();
  });

  test('a name is that field of the focused panel, focused or not', () => {
    const { finder, filter, root } = makeRoot();
    expect(resolveHistoryField('finder', filter, root)).toBe(finder);
    expect(resolveHistoryField('filter', finder, root)).toBeNull();
    expect(resolveHistoryField('nothing', finder, root)).toBeNull();
    expect(resolveHistoryField('finder', finder, null)).toBeNull();
  });

  test('a field whose zone is switched off has no history for now', () => {
    const { finder, root } = makeRoot(() => null);
    expect(resolveHistoryField('self-focus', finder, root)).toBeNull();
  });
});

describe('historyFieldNames', () => {
  test('lists the named fields of the panel that have a history', () => {
    const { root } = makeRoot();
    expect(historyFieldNames(root)).toEqual(['finder']);
    expect(historyFieldNames(null)).toEqual([]);
  });
});

describe('historyApplies: a key on a history command', () => {
  test('passes through where the focused field has no history', () => {
    const { finder, filter, root } = makeRoot();
    expect(historyApplies('history:prev self-focus', finder, root)).toBe(true);
    expect(historyApplies('history:prev self-focus', filter, root)).toBe(false);
    expect(historyApplies('history:search self-focus', document.body, root)).toBe(false);
    expect(historyApplies('history:search finder', filter, root)).toBe(true);
  });

  test('says nothing of any other command', () => {
    expect(historyApplies('metarecord:create', null, null)).toBe(true);
  });
});

describe('runHistoryCommand', () => {
  test('prev and next walk the focused field', async () => {
    const { finder, root } = makeRoot();
    finder.focus();
    finder.value = 'draft';
    await runHistoryCommand('history:prev', 'self-focus', finder, root);
    expect(finder.value).toBe('two');
    await runHistoryCommand('history:next', 'self-focus', finder, root);
    expect(finder.value).toBe('draft');
  });

  test('a name gives the field the focus, and its search opens', async () => {
    const { finder, filter, root } = makeRoot();
    filter.focus();
    await runHistoryCommand('history:search', 'finder', filter, root);
    await flush();
    const search = root.querySelector('.mf-history-search');
    expect(search).not.toBeNull();
    expect(root.querySelector('.mf-history-overlay')?.textContent).toContain('two');
    expect(finder.value).toBe('');
  });

  test('a name walks that field and leaves the focus on it', async () => {
    const { finder, filter, root } = makeRoot();
    filter.focus();
    await runHistoryCommand('history:prev', 'finder', filter, root);
    expect(finder.value).toBe('two');
    expect(document.activeElement).toBe(finder);
  });

  test('the argument is required, and a field without history says so', async () => {
    const { filter, root } = makeRoot();
    await expect(runHistoryCommand('history:prev', undefined, filter, root)).rejects.toThrow(
      /which field/,
    );
    await expect(runHistoryCommand('history:prev', 'self-focus', filter, root)).rejects.toThrow(
      /no text field with a history has the focus/,
    );
    await expect(runHistoryCommand('history:prev', 'filter', filter, root)).rejects.toThrow(
      /no field "filter" with a history in the focused panel/,
    );
  });
});

describe('last-edited: the field of the focused panel last edited', () => {
  /** A panel's Shadow root holding two fields with a history. */
  function makePanel() {
    const host = document.createElement('div');
    document.body.appendChild(host);
    const root = host.attachShadow({ mode: 'open' });
    const container = document.createElement('div');
    root.appendChild(container);
    const field = (name: string, entries: string[]) => {
      const input = document.createElement('input');
      input.dataset.mfFocus = name;
      container.appendChild(input);
      attachHistory(input, {
        zone: `test:${name}`,
        read: async () => entries,
        append: async () => true,
        getRepo: async () => 'repo',
        container,
      });
      return input;
    };
    return { root, finder: field('finder', ['f1', 'f2']), query: field('query', ['q1', 'q2']) };
  }

  function edit(input: HTMLInputElement, value: string) {
    input.value = value;
    input.dispatchEvent(new Event('input', { bubbles: true }));
  }

  test('is the field edited last, and nothing before any edit', () => {
    const { root, finder, query } = makePanel();
    expect(resolveHistoryField('last-edited', document.body, root)).toBeNull();
    edit(finder, 'a');
    expect(resolveHistoryField('last-edited', document.body, root)).toBe(finder);
    edit(query, 'b');
    expect(resolveHistoryField('last-edited', document.body, root)).toBe(query);
  });

  test('a recall counts as an edit of that field', async () => {
    const { root, finder, query } = makePanel();
    edit(query, 'b');
    await runHistoryCommand('history:prev', 'finder', document.body, root);
    expect(resolveHistoryField('last-edited', document.body, root)).toBe(finder);
  });

  test('a key on it passes through until a field was edited', () => {
    const { root, finder } = makePanel();
    expect(historyApplies('history:prev last-edited', document.body, root)).toBe(false);
    edit(finder, 'a');
    expect(historyApplies('history:prev last-edited', document.body, root)).toBe(true);
  });

  test('a field gone from the panel is no longer designated', () => {
    const { root, finder } = makePanel();
    edit(finder, 'a');
    finder.remove();
    expect(resolveHistoryField('last-edited', document.body, root)).toBeNull();
  });

  test('walks that field from outside it, and gives it the focus', async () => {
    const { root, finder, query } = makePanel();
    edit(finder, 'a');
    edit(query, 'b');
    query.blur();
    await runHistoryCommand('history:prev', 'last-edited', document.body, root);
    expect(query.value).toBe('q2');
    expect(root.activeElement).toBe(query);
    expect(finder.value).toBe('a');
  });

  test('without an edited field, says so', async () => {
    const { root } = makePanel();
    await expect(
      runHistoryCommand('history:prev', 'last-edited', document.body, root),
    ).rejects.toThrow(/no field with a history was edited in the focused panel/);
  });
});

describe('the shipped keys', () => {
  const toml = readFileSync(resolve(process.cwd(), '../default-config/keybindings.toml'), 'utf8');

  test.each([
    ['ctrl+p', 'history:prev self-focus'],
    ['ctrl+n', 'history:next self-focus'],
    ['ctrl+r', 'history:search self-focus'],
  ])('%s runs %s, also in a text field', (key, command) => {
    const start = toml.indexOf(`"${key}" = `);
    expect(start).toBeGreaterThanOrEqual(0);
    const block = toml.slice(start, toml.indexOf('\n"', start + 1));
    expect(block).toContain(`{ command = "${command}", text-input = true }`);
  });

  test('ctrl+p outside a text field recalls in the field last edited', () => {
    const start = toml.indexOf('"ctrl+p" = ');
    const block = toml.slice(start, toml.indexOf('\n"', start + 1));
    expect(block).toContain('{ command = "history:prev last-edited" }');
  });
});
