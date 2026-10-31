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

describe('self: the walk over the fields the panel last changed', () => {
  /** A panel's Shadow root holding two fields with a history, and its
   *  `fields` store holding `recorded`. */
  function makePanel(recorded: string[] = []) {
    const host = document.createElement('div');
    document.body.appendChild(host);
    const root = host.attachShadow({ mode: 'open' });
    const container = document.createElement('div');
    root.appendChild(container);
    const fieldsFile = [...recorded];
    const stores: Record<string, string[]> = {
      'test:finder': ['f1', 'f2'],
      'test:query': ['q1', 'q2'],
      'test:fields': fieldsFile,
    };
    const field = (name: string) => {
      const input = document.createElement('input');
      input.dataset.mfFocus = name;
      container.appendChild(input);
      const attached = attachHistory(input, {
        zone: `test:${name}`,
        read: async (_repo: string, zone: string) => stores[zone],
        append: async (_repo: string, zone: string, entry: string) => {
          if (stores[zone].at(-1) === entry) return false;
          stores[zone] = [...stores[zone], entry];
          return true;
        },
        getRepo: async () => 'repo',
        container,
        fields: {
          zone: 'test:fields',
          append: async (_repo: string, _zone: string, name: string) => {
            fieldsFile.push(name);
          },
        },
      });
      return { input, attached };
    };
    return { root, finder: field('finder'), query: field('query'), fieldsFile };
  }

  test('each step recalls in the field at that place of the list', async () => {
    const { root, finder, query } = makePanel(['finder', 'query', 'finder']);
    await runHistoryCommand('history:prev', 'self', document.body, root);
    expect([finder.input.value, query.input.value]).toEqual(['f2', '']);
    await runHistoryCommand('history:prev', 'self', document.body, root);
    expect([finder.input.value, query.input.value]).toEqual(['f2', 'q2']);
    await runHistoryCommand('history:prev', 'self', document.body, root);
    expect([finder.input.value, query.input.value]).toEqual(['f1', 'q2']);
  });

  test('next walks the same steps back', async () => {
    const { root, finder, query } = makePanel(['finder', 'query']);
    finder.input.value = 'typed';
    await runHistoryCommand('history:prev', 'self', document.body, root);
    await runHistoryCommand('history:prev', 'self', document.body, root);
    expect([finder.input.value, query.input.value]).toEqual(['f2', 'q2']);
    await runHistoryCommand('history:next', 'self', document.body, root);
    expect([finder.input.value, query.input.value]).toEqual(['typed', 'q2']);
    await runHistoryCommand('history:next', 'self', document.body, root);
    expect([finder.input.value, query.input.value]).toEqual(['typed', '']);
    await runHistoryCommand('history:next', 'self', document.body, root);
    expect([finder.input.value, query.input.value]).toEqual(['typed', '']);
  });

  test('gives no field the focus', async () => {
    const { root } = makePanel(['query']);
    await runHistoryCommand('history:prev', 'self', document.body, root);
    expect(root.activeElement).toBeNull();
  });

  test('keys pressed quickly each take their step', async () => {
    const { root, finder, query } = makePanel(['finder', 'query']);
    await Promise.all([
      runHistoryCommand('history:prev', 'self', document.body, root),
      runHistoryCommand('history:prev', 'self', document.body, root),
    ]);
    expect([finder.input.value, query.input.value]).toEqual(['f2', 'q2']);
  });

  test('a submit records the field, only when its entry is new', async () => {
    const { query, fieldsFile } = makePanel();
    query.input.value = 'new';
    query.attached.push('new');
    await flush();
    query.attached.push('new');
    await flush();
    expect(fieldsFile).toEqual(['query']);
  });

  test('a real edit ends the walk, in every field', async () => {
    const { root, finder, query } = makePanel(['finder', 'query']);
    await runHistoryCommand('history:prev', 'self', document.body, root);
    await runHistoryCommand('history:prev', 'self', document.body, root);
    finder.input.value = 'edited';
    finder.input.dispatchEvent(new Event('input', { bubbles: true }));
    query.input.value = '';
    await runHistoryCommand('history:prev', 'self', document.body, root);
    expect(query.input.value).toBe('q2');
  });

  test('a field the panel no longer has is passed over', async () => {
    const { root, finder, query } = makePanel(['query', 'gone']);
    await runHistoryCommand('history:prev', 'self', document.body, root);
    expect(query.input.value).toBe('q2');
    expect(finder.input.value).toBe('');
  });

  test('a key on it passes through in a panel without such history', () => {
    const { root } = makePanel();
    expect(historyApplies('history:prev self', document.body, root)).toBe(true);
    const plain = document.createElement('div').attachShadow({ mode: 'open' });
    expect(historyApplies('history:prev self', document.body, plain)).toBe(false);
    expect(historyApplies('history:next self', document.body, null)).toBe(false);
  });

  test('without such history, and for a search, says so', async () => {
    const { root } = makePanel();
    const plain = document.createElement('div').attachShadow({ mode: 'open' });
    await expect(runHistoryCommand('history:prev', 'self', document.body, plain)).rejects.toThrow(
      /the focused panel has no history/,
    );
    await expect(runHistoryCommand('history:search', 'self', document.body, root)).rejects.toThrow(
      /history:search needs a field/,
    );
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

  test.each([
    ['ctrl+p', 'history:prev self'],
    ['ctrl+n', 'history:next self'],
  ])('%s outside a text field runs %s', (key, command) => {
    const start = toml.indexOf(`"${key}" = `);
    const block = toml.slice(start, toml.indexOf('\n"', start + 1));
    expect(block).toContain(`{ command = "${command}" }`);
    expect(block.match(/command = /g)).toHaveLength(2);
  });
});
