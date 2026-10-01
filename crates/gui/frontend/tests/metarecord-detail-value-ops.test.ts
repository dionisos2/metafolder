// Naming a value to act on — `metarecord:field remove` and
// `metarecord:field edit` (spec-gui "metarecord-detail panel type"): the field
// first, then the value. `remove` is the exact inverse of `add` (every row
// equal to the value it names is deleted), and `edit` names the row to change
// and its replacement. Both travel the same raw vocabulary values are written
// in — so a `ref` with a completion seed (doc "Ref value seeds") is
// named by its PATH in the seed forest rather than its uuid, on the way in
// (parsed as one being added) and on the way out (the candidates offered, and
// the value an edit starts from). This replaces row picks whose labels spelled
// every ref out as a uuid — labels that could not be typed inline at all.
//
// The panel is mounted the way the shell mounts it (panel-mount.test.ts), its
// registered argument specs are driven through the real `collectArgs`, and the
// recorded daemon calls are the assertion.

import { describe, expect, test, vi, beforeEach } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { collectArgs } from '../src/lib/commands';
import type { ArgSpec } from '../src/lib/commands';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/metarecord-detail');

const REPO = 'repo-1';
const UUID = 'uuid-1';
// Real 32-hex uuids: `resolveRefValue` takes one as a uuid verbatim.
const U_JAZZ = 'a'.repeat(32);
const U_BLUES = 'b'.repeat(32);
const U_PARENT = 'c'.repeat(32);

type Field = { id: number; name: string; value: { type: string; value?: unknown } };
type Spec = { args?: ArgSpec[]; handler: (...args: string[]) => unknown };
type Call = { method: string; path: string; body: unknown };

function shadowFor(): ShadowRoot {
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

/** Mounts the panel on a metarecord holding `fields`.
 *  `seeds` is the `[ref-completion-seeds]` map (ref field → tree_ref field);
 *  `tree` maps a path to the metarecord uuid it resolves to — read backwards
 *  for the path read-back (`treePaths`) and forwards for path resolution
 *  (`/tree/resolve-path`), the two directions `resolveRefValue` and
 *  `rawOfValue` travel. `treePaths` answers the tree_ref read-back
 *  (`…/fields/:field/resolve-tree`). */
async function mountPanel(
  fields: Field[],
  {
    catalog = {},
    vars = {},
    seeds = {},
    tree = {},
    treePaths = ['/a/b'],
  }: {
    catalog?: Record<string, string>;
    vars?: Record<string, unknown>;
    seeds?: Record<string, string>;
    tree?: Record<string, string>;
    treePaths?: string[];
  } = {},
) {
  const specs = new Map<string, Spec>();
  const calls: Call[] = [];
  const noop = () => {};
  const store = new Map<string, unknown>([
    ['selected_metarecord', { uuid: UUID, repo: REPO }],
    ...Object.entries(vars),
  ]);
  const byUuid = Object.fromEntries(Object.entries(tree).map(([path, uuid]) => [uuid, path]));
  const api = {
    ready: Promise.resolve(),
    workspaceId: 'ws-1',
    panelType: 'metarecord-detail',
    guiServer: 'http://127.0.0.1:7524',
    sessionToken: 'token',
    pageSize: 100,
    settings: {},
    defaults: {},
    visible: true,
    onVisibility: noop,
    whenVisible: (fn: () => unknown) => void fn(),
    bench: { measure: (_n: string, fn: () => unknown) => fn(), record: noop },
    daemon: {
      query: async () => ({ uuids: [], nextCursor: null, total: 0 }),
      treePaths: async (_repo: string, _field: string, uuids: string[]) =>
        Object.fromEntries(uuids.map((u) => [u, byUuid[u] ? [byUuid[u]] : []])),
      metarecords: async () => new Map(),
      fields: async () => Object.entries(catalog).map(([name, type]) => ({ name, type })),
      request: async () => ({ status: 200, body: null }),
      call: async (method: string, path: string, body: unknown = null) => {
        calls.push({ method, path, body });
        if (method === 'GET' && path === `/repos/${REPO}/metarecords/${UUID}`)
          return { uuid: UUID, version: 1, fields };
        if (method === 'GET' && path.endsWith('/resolve-tree')) return { paths: treePaths };
        // The whole-forest path listing behind a value completion: empty —
        // these tests name values, they do not complete over the forest.
        if (method === 'POST' && path.endsWith('/resolve-tree')) return {};
        if (method === 'POST' && path.endsWith('/tree/resolve-path')) {
          const { path: p } = body as { field: string; path: string };
          return { uuid: tree[p] ?? null };
        }
        return null;
      },
      resolvePath: async () => '',
      resolveTreeRef: async () => '',
      repoRoot: async () => '/tmp/repo',
      repoInternalDir: async () => '/tmp/repo/.metafolder/internal',
      metarecordPaths: async () => [],
    },
    changes: { sync: async () => {}, subscribe: () => () => {} },
    query: { parse: async () => null, expand: async () => '', grammarSource: async () => '' },
    pick: { start: async () => '' },
    config: {
      pickerSeed: async () => null,
      refCompletionSeed: async (field: string) => seeds[field] ?? null,
      refSeed: async () => null,
      labelSeparator: async () => ' | ',
    },
    recent: { touch: async () => {}, list: async () => [] },
    workspace: {
      get: async (key: string) => store.get(key) ?? null,
      set: async (key: string, value: unknown) => void store.set(key, value),
      adoptRepo: async () => {},
      onChange: noop,
    },
    commands: {
      register: async (name: string, spec: Spec) => void specs.set(name, spec),
      invoke: () => null,
    },
    addKeybinding: async () => null,
    fs: {
      readDir: async () => [],
      stat: async () => ({}),
      exists: async () => true,
      homeDir: async () => '/home/user',
    },
    trash: { list: async () => [], restore: async () => '', remove: async () => {}, empty: async () => 0 },
    history: { read: async () => [], append: async () => {} },
    statusBar: { message: async () => {}, error: async () => {} },
    messages: { list: async () => [], append: async () => {}, onAppend: noop },
    contextMenu: Object.assign(noop, { addDefaultItems: noop }),
  };
  const mod = await import('../../default-config/panel-types/metarecord-detail/main.js');
  await mod.mount(shadowFor(), api as never);
  calls.length = 0; // drop the initial load
  return { specs, calls };
}

/** Collects `invocation`'s missing arguments, answering each prompt in turn
 *  with `answers`; returns the prompts shown, the final arguments, each
 *  prompt's completion candidates and its pre-filled value. */
async function collect(spec: Spec, provided: string[], answers: string[]) {
  const prompts: string[] = [];
  const completions: (string[] | Promise<string[]>)[] = [];
  const initials: (string | undefined)[] = [];
  const queue = [...answers];
  const args = await collectArgs(spec.args ?? [], provided, async (request) => {
    prompts.push(request.prompt);
    // The recorded candidates are the labels the list shows (doc
    // "Completion views"); the values ride along in the live source.
    completions.push(
      Promise.resolve(request.completions).then((items) =>
        items.map((item) => (typeof item === 'string' ? item : item.label)),
      ),
    );
    initials.push(request.initial);
    return queue.shift() ?? null;
  });
  return { prompts, args, completions, initials };
}

beforeEach(() => {
  vi.stubGlobal(
    'fetch',
    vi.fn(async () => new Response('[]', { status: 200 })),
  );
  document.body.replaceChildren();
});

/** Two `tag` refs, seeded against a `path` forest — the ref case the
 *  completion seed exists for. */
const tagFields = (): Field[] => [
  { id: 1, name: 'tag', value: { type: 'ref', value: U_JAZZ } },
  { id: 2, name: 'tag', value: { type: 'ref', value: U_BLUES } },
];
const tagSeeds = { tag: 'path' };
const tagTree = { '/fruits/jazz': U_JAZZ, '/fruits/blues': U_BLUES };

describe('metarecord:field remove — the field, then the value', () => {
  test('the value is asked after the field name — and no type', async () => {
    // A removable row carries the type its value is read as: nothing settles a
    // type here, yet there is none to ask for.
    const { specs } = await mountPanel(tagFields(), { seeds: tagSeeds, tree: tagTree });
    const { prompts, args, completions } = await collect(specs.get('metarecord:field')!, ['remove'], [
      'tag',
      '/fruits/jazz',
    ]);
    expect(prompts).toEqual(['Field to remove a value from?', 'Value to remove from "tag"?']);
    expect(args).toEqual(['remove', 'tag', '/fruits/jazz']);
    expect(await completions[0]).toEqual(['tag']);
  });

  test('the candidates are the record’s own values, a seeded ref read back as its path', async () => {
    const { specs } = await mountPanel(tagFields(), { seeds: tagSeeds, tree: tagTree });
    const { completions, initials } = await collect(specs.get('metarecord:field')!, ['remove'], [
      'tag',
      '',
    ]);
    expect(await completions[1]).toEqual(['/fruits/jazz', '/fruits/blues']);
    // Two values, so nothing is pre-filled: the pick is the answer.
    expect(initials[1]).toBe('');
  });

  test('a single value is pre-filled — in the same raw form it is entered in', async () => {
    const { specs } = await mountPanel([tagFields()[0]], { seeds: tagSeeds, tree: tagTree });
    const { initials } = await collect(specs.get('metarecord:field')!, ['remove'], ['tag', '']);
    expect(initials[1]).toBe('/fruits/jazz');
  });

  test('a value named by its seed path removes the row it resolves to', async () => {
    const { specs, calls } = await mountPanel(tagFields(), { seeds: tagSeeds, tree: tagTree });
    await specs.get('metarecord:field')!.handler('remove', 'tag', '/fruits/jazz');
    expect(calls.filter((c) => c.method === 'DELETE')).toEqual([
      { method: 'DELETE', path: `/repos/${REPO}/fields/1`, body: null },
    ]);
  });

  test('an explicit uuid names a ref just the same', async () => {
    const { specs, calls } = await mountPanel(tagFields(), { seeds: tagSeeds, tree: tagTree });
    await specs.get('metarecord:field')!.handler('remove', 'tag', U_BLUES);
    expect(calls.filter((c) => c.method === 'DELETE')).toEqual([
      { method: 'DELETE', path: `/repos/${REPO}/fields/2`, body: null },
    ]);
  });

  test('every row equal to the value goes — the inverse of add', async () => {
    const fields: Field[] = [
      { id: 3, name: 'rating', value: { type: 'int', value: 5 } },
      { id: 4, name: 'rating', value: { type: 'int', value: 5 } },
      { id: 5, name: 'rating', value: { type: 'int', value: 7 } },
    ];
    const { specs, calls } = await mountPanel(fields);
    await specs.get('metarecord:field')!.handler('remove', 'rating', '5');
    expect(calls.filter((c) => c.method === 'DELETE').map((c) => c.path)).toEqual([
      `/repos/${REPO}/fields/3`,
      `/repos/${REPO}/fields/4`,
    ]);
  });

  test('∅ names the explicit absences, alongside whatever reads exactly so', async () => {
    const fields: Field[] = [
      { id: 6, name: 'note', value: { type: 'nothing' } },
      { id: 7, name: 'note', value: { type: 'string', value: 'hi' } },
    ];
    const { specs, calls } = await mountPanel(fields);
    const { completions } = await collect(specs.get('metarecord:field')!, ['remove'], ['note', '']);
    expect(await completions[1]).toEqual(['∅', 'hi']);
    await specs.get('metarecord:field')!.handler('remove', 'note', '∅');
    expect(calls.filter((c) => c.method === 'DELETE').map((c) => c.path)).toEqual([
      `/repos/${REPO}/fields/6`,
    ]);
  });

  test('a tree_ref value is named by its path, parent and name resolved back', async () => {
    const fields: Field[] = [
      { id: 8, name: 'location', value: { type: 'tree_ref', value: { parent: U_PARENT, name: 'b' } } },
    ];
    const { specs, calls } = await mountPanel(fields, { tree: { '/a': U_PARENT } });
    const { completions } = await collect(specs.get('metarecord:field')!, ['remove'], [
      'location',
      '',
    ]);
    expect(await completions[1]).toEqual(['/a/b']);
    await specs.get('metarecord:field')!.handler('remove', 'location', '/a/b');
    expect(calls.filter((c) => c.method === 'DELETE')).toEqual([
      { method: 'DELETE', path: `/repos/${REPO}/fields/8`, body: null },
    ]);
  });

  test('a value the record does not hold is refused', async () => {
    const { specs } = await mountPanel(tagFields(), { seeds: tagSeeds, tree: tagTree });
    await expect(
      specs.get('metarecord:field')!.handler('remove', 'tag', '/fruits/rock'),
    ).rejects.toThrow('no value "/fruits/rock" on "tag"');
  });

  test('an unknown field name is refused', async () => {
    const { specs } = await mountPanel(tagFields(), { seeds: tagSeeds, tree: tagTree });
    await expect(specs.get('metarecord:field')!.handler('remove', 'nope', 'x')).rejects.toThrow(
      'no field "nope"',
    );
  });
});

describe('metarecord:field edit — the field, the row, the new value', () => {
  test('one row is named by the field alone', async () => {
    const fields = [{ id: 3, name: 'rating', value: { type: 'int', value: 5 } }];
    const { specs, calls } = await mountPanel(fields);
    const { prompts, args, initials } = await collect(specs.get('metarecord:field')!, ['edit'], [
      'rating',
      '7',
    ]);
    expect(prompts).toEqual(['Field to edit a value of?', 'New value for "rating"?']);
    expect(args).toEqual(['edit', 'rating', '7']);
    // The edit starts from the value being replaced, as it reads.
    expect(initials[1]).toBe('5');
    await specs.get('metarecord:field')!.handler(...args!);
    expect(calls).toContainEqual({
      method: 'PATCH',
      path: `/repos/${REPO}/fields/3`,
      body: { value: { type: 'int', value: 7 } },
    });
  });

  test('several rows: which one is named in between, readably', async () => {
    const { specs } = await mountPanel(tagFields(), { seeds: tagSeeds, tree: tagTree });
    const { prompts, args, completions, initials } = await collect(
      specs.get('metarecord:field')!,
      ['edit'],
      ['tag', '/fruits/jazz', '/fruits/rock'],
    );
    expect(prompts).toEqual([
      'Field to edit a value of?',
      'Which value of "tag" to edit?',
      'New value for "tag"?',
    ]);
    expect(await completions[1]).toEqual(['/fruits/jazz', '/fruits/blues']);
    // The new value starts from the chosen one, as it reads.
    expect(initials[2]).toBe('/fruits/jazz');
    expect(args).toEqual(['edit', 'tag', '/fruits/jazz', '/fruits/rock']);
  });

  test('the replacement is read as the row’s type — a seeded ref by its path', async () => {
    const { specs, calls } = await mountPanel(tagFields(), { seeds: tagSeeds, tree: tagTree });
    await specs.get('metarecord:field')!.handler('edit', 'tag', '/fruits/jazz', '/fruits/blues');
    expect(calls).toContainEqual({
      method: 'PATCH',
      path: `/repos/${REPO}/fields/1`,
      body: { value: { type: 'ref', value: U_BLUES } },
    });
  });

  test('a Nothing row takes its type from the type argument', async () => {
    // An absence carries no type of its own: giving it a value is what
    // establishes one, so the type is asked — and the row needs no `which`.
    const fields = [{ id: 9, name: 'rating', value: { type: 'nothing' } }];
    const { specs, calls } = await mountPanel(fields);
    const { prompts, args } = await collect(specs.get('metarecord:field')!, ['edit'], [
      'rating',
      'int',
      '5',
    ]);
    expect(prompts).toEqual([
      'Field to edit a value of?',
      'Type for "rating"?',
      'New value for "rating"?',
    ]);
    await specs.get('metarecord:field')!.handler(...args!);
    expect(calls).toContainEqual({
      method: 'PATCH',
      path: `/repos/${REPO}/fields/9`,
      body: { value: { type: 'int', value: 5 } },
    });
  });

  test('a row the field does not hold names nothing', async () => {
    const { specs } = await mountPanel(tagFields(), { seeds: tagSeeds, tree: tagTree });
    await expect(
      specs.get('metarecord:field')!.handler('edit', 'tag', '/fruits/rock', 'x'),
    ).rejects.toThrow('no value "/fruits/rock" on "tag"');
  });
});
