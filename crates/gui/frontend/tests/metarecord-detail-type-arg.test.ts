// The value *type* is an ordinary command argument (spec-gui "metarecord-detail
// panel type"): when nothing settles what type a field's value should have,
// `metarecord:field` and `metarecord:bulk` ask for it through the command
// input — with completion over the concrete types, and *before* the value,
// since the value is parsed as that type. It used to be a `window.prompt`
// popup opened after the value had already been typed.
//
// The panel is mounted the way the shell mounts it (panel-mount.test.ts), its
// registered argument specs are driven through the real `collectArgs`, and the
// recorded prompts are the assertion.

import { describe, expect, test, vi, beforeEach } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { collectArgs } from '../src/lib/commands';
import type { ArgSpec } from '../src/lib/commands';

const PANEL_DIR = resolve(process.cwd(), '../default-config/panel-types/metarecord-detail');

const REPO = 'repo-1';
const UUID = 'uuid-1';

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

/** Mounts the panel on a metarecord holding `fields`, with `catalog` as the
 *  repo's field catalogue (the schema-aware `GET /repos/:repo/fields`), and
 *  `vars` published as workspace variables (e.g. `selected_metarecords`). */
async function mountPanel(fields: Field[], catalog: Record<string, string> = {}, vars: Record<string, unknown> = {}) {
  const specs = new Map<string, Spec>();
  const calls: Call[] = [];
  const noop = () => {};
  const store = new Map<string, unknown>([
    ['selected_metarecord', { uuid: UUID, repo: REPO }],
    ...Object.entries(vars),
  ]);
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
        Object.fromEntries(uuids.map((uuid) => [uuid, [] as string[]])),
      metarecords: async () => new Map(),
      fields: async () => Object.entries(catalog).map(([name, type]) => ({ name, type })),
      request: async () => ({ status: 200, body: null }),
      call: async (method: string, path: string, body: unknown = null) => {
        calls.push({ method, path, body });
        if (method === 'GET' && path === `/repos/${REPO}/metarecords/${UUID}`)
          return { uuid: UUID, version: 1, fields };
        // A bulk target's COUNT — two matches, whatever the query — and the bulk
        // writes themselves report back the changed rows.
        if (body && (body as { count?: boolean }).count === true) return { total: 2 };
        if (method === 'POST' && path.endsWith('/query/delete')) return { deleted: 2 };
        if (method === 'POST' && path.includes('/query/fields/')) return { updated: 2 };
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
 *  with `answers`; returns the prompts that were shown, the final arguments,
 *  and each prompt's recorded `initial` (pre-filled) value. */
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

describe('metarecord:field — the type is an argument', () => {
  test('an unsettled type is asked through the command input, before the value', async () => {
    const { specs } = await mountPanel([]);
    const { prompts, args, completions } = await collect(
      specs.get('metarecord:field')!,
      ['set'],
      ['rating', 'int', '5'],
    );
    expect(prompts).toEqual(['Field to set?', 'Type for "rating"?', 'Value for "rating"?']);
    expect(args).toEqual(['set', 'rating', 'int', '5']);
    // Completed over the concrete types — a pick, not free text typed into a popup.
    expect(await completions[1]).toContain('int');
    expect(await completions[1]).not.toContain('nothing');
  });

  test('a type the repo catalogue already knows is not asked', async () => {
    const { specs } = await mountPanel([], { rating: 'int' });
    const { prompts, args } = await collect(
      specs.get('metarecord:field')!,
      ['set'],
      ['rating', '5'],
    );
    expect(prompts).toEqual(['Field to set?', 'Value for "rating"?']);
    expect(args).toEqual(['set', 'rating', '5']);
  });

  test("a type the record's own row carries is not asked", async () => {
    const fields = [{ id: 1, name: 'rating', value: { type: 'int', value: 3 } }];
    const { specs } = await mountPanel(fields);
    const { prompts } = await collect(specs.get('metarecord:field')!, ['set'], ['rating', '5']);
    expect(prompts).toEqual(['Field to set?', 'Value for "rating"?']);
  });

  test('the picked type is what the value is written as', async () => {
    const { specs, calls } = await mountPanel([]);
    await specs.get('metarecord:field')!.handler('set', 'rating', 'int', '5');
    expect(calls).toContainEqual({
      method: 'PUT',
      path: `/repos/${REPO}/metarecords/${UUID}/fields/rating`,
      body: { value: { type: 'int', value: 5 } },
    });
  });

  test('a settled type still reaches the write when no type argument was given', async () => {
    const { specs, calls } = await mountPanel([], { rating: 'int' });
    await specs.get('metarecord:field')!.handler('set', 'rating', '5');
    expect(calls).toContainEqual({
      method: 'PUT',
      path: `/repos/${REPO}/metarecords/${UUID}/fields/rating`,
      body: { value: { type: 'int', value: 5 } },
    });
  });

  test('an operation that writes no value is unaffected', async () => {
    const fields = [{ id: 7, name: 'tag', value: { type: 'string', value: 'jazz' } }];
    const { specs } = await mountPanel(fields);
    const { prompts, args } = await collect(
      specs.get('metarecord:field')!,
      ['rename'],
      ['tag', 'label'],
    );
    expect(prompts).toEqual(['Field to rename?', 'Rename "tag" to?']);
    expect(args).toEqual(['rename', 'tag', 'label']);
  });

  test('retyping a Nothing-only field asks its value through the command input', async () => {
    // The type it is being given is only meaningful with a value, and there is
    // no row to re-encode into it — so the value follows the type as an
    // ordinary argument (it used to be a `window.prompt` popup).
    const fields = [{ id: 9, name: 'rating', value: { type: 'nothing' } }];
    const { specs, calls } = await mountPanel(fields);
    const { prompts, args } = await collect(
      specs.get('metarecord:field')!,
      ['retype'],
      ['rating', 'int', '5'],
    );
    expect(prompts).toEqual([
      'Field to retype?',
      'New type for "rating"?',
      'Value for "rating" (int)?',
    ]);
    await specs.get('metarecord:field')!.handler(...args!);
    expect(calls).toContainEqual({
      method: 'PATCH',
      path: `/repos/${REPO}/fields/9`,
      body: { value: { type: 'int', value: 5 } },
    });
  });

  test('retype keeps asking for a type as its own value argument', async () => {
    const fields = [{ id: 7, name: 'rating', value: { type: 'string', value: '3' } }];
    const { specs } = await mountPanel(fields);
    const { prompts, args } = await collect(
      specs.get('metarecord:field')!,
      ['retype'],
      ['rating', 'int'],
    );
    expect(prompts).toEqual(['Field to retype?', 'New type for "rating"?']);
    expect(args).toEqual(['retype', 'rating', 'int']);
  });
});

describe('metarecord:bulk — the target is the first argument', () => {
  test('an unsettled type is asked before the value', async () => {
    const { specs } = await mountPanel([]);
    const { prompts, args } = await collect(
      specs.get('metarecord:bulk')!,
      ['selection', 'set'],
      ['rating', 'int', '5'],
    );
    expect(prompts).toEqual(['Field to set?', 'Type for "rating"?', 'Value for "rating"?']);
    expect(args).toEqual(['selection', 'set', 'rating', 'int', '5']);
  });

  test('a catalogued type is not asked', async () => {
    const { specs } = await mountPanel([], { rating: 'int' });
    const { prompts } = await collect(specs.get('metarecord:bulk')!, ['selection', 'set'], [
      'rating',
      '5',
    ]);
    expect(prompts).toEqual(['Field to set?', 'Value for "rating"?']);
  });

  test('unset takes neither a type nor a value', async () => {
    const { specs } = await mountPanel([], { rating: 'int' });
    const { prompts, args } = await collect(specs.get('metarecord:bulk')!, ['selection', 'unset'], [
      'rating',
    ]);
    expect(prompts).toEqual(['Field to remove?']);
    expect(args).toEqual(['selection', 'unset', 'rating']);
  });

  test('delete takes no field at all', async () => {
    const { specs } = await mountPanel([]);
    const { prompts, args } = await collect(specs.get('metarecord:bulk')!, ['query', 'delete'], []);
    expect(prompts).toEqual([]);
    expect(args).toEqual(['query', 'delete']);
  });

  test('a bare invocation asks the target first, with completion and a pre-fill', async () => {
    const { specs } = await mountPanel([], { rating: 'int' });
    const { prompts, args, completions, initials } = await collect(
      specs.get('metarecord:bulk')!,
      [],
      ['selection', 'set', 'rating', '5'],
    );
    expect(prompts[0]).toMatch(/^Target\?/);
    expect(await completions[0]).toEqual(['selection', 'query']);
    // Nothing is checked, so the pre-fill is the query — the old implicit
    // default, now a visible, editable answer instead of a silent fallback.
    expect(initials[0]).toBe('query');
    expect(prompts.slice(1)).toEqual([
      'Operation? (set / add / remove / unset / delete)',
      'Field to set?',
      'Value for "rating"?',
    ]);
    expect(args).toEqual(['selection', 'set', 'rating', '5']);
  });

  test('the target prompt names what each choice would act on', async () => {
    // Two checked records; the list has published nothing, so the query branch
    // reads as ALL metarecords (the daemon stub answers no COUNT — 0).
    const { specs } = await mountPanel([], { rating: 'int' }, { selected_metarecords: ['a', 'b'] });
    const { prompts, initials } = await collect(specs.get('metarecord:bulk')!, [], [
      'selection',
      'set',
      'rating',
      '5',
    ]);
    expect(prompts[0]).toBe('Target? (selection = 2 checked · query = ALL metarecords (2))');
    // A checked selection pre-fills `selection`.
    expect(initials[0]).toBe('selection');
  });

  test('an inline target and operation are not asked again', async () => {
    const { specs } = await mountPanel([], { rating: 'int', tag: 'string' });
    const { prompts, args } = await collect(specs.get('metarecord:bulk')!, ['query', 'add'], [
      'tag',
      'jazz',
    ]);
    expect(prompts).toEqual(['Field to add a value to?', 'Value to add to "tag"?']);
    expect(args).toEqual(['query', 'add', 'tag', 'jazz']);
  });

  test('an unknown target is refused, naming the new syntax', async () => {
    // What the old positional form reads as — `metarecord:bulk set rating 5`
    // lands here with target "set" — must fail loudly, not silently re-target.
    const { specs } = await mountPanel([]);
    await expect(specs.get('metarecord:bulk')!.handler('set', 'rating', '5')).rejects.toThrow(
      'unknown target "set"',
    );
  });

  test('an empty selection is nothing to do, never the query', async () => {
    const { specs, calls } = await mountPanel([]);
    await specs.get('metarecord:bulk')!.handler('selection', 'delete');
    expect(calls.filter((c) => c.method === 'POST')).toEqual([]);
  });

  test('the selection target spells the checked UUIDs as the query', async () => {
    const { specs, calls } = await mountPanel([], {}, { selected_metarecords: ['uuid-a', 'uuid-b'] });
    vi.stubGlobal('confirm', vi.fn(() => true));
    await specs.get('metarecord:bulk')!.handler('selection', 'delete');
    const post = calls.find((c) => c.method === 'POST' && c.path.endsWith('/query/delete'));
    expect(post?.body).toEqual({ query: { type: 'uuid_in', uuids: ['uuid-a', 'uuid-b'] } });
  });

  test('the query target runs over the effective query', async () => {
    const { specs, calls } = await mountPanel(
      [],
      { rating: 'int' },
      { 'metarecord-list:effective-query': { type: 'match', field: 'rating', op: '>', value: 3 } },
    );
    vi.stubGlobal('confirm', vi.fn(() => true));
    await specs.get('metarecord:bulk')!.handler('query', 'unset', 'rating');
    const post = calls.find((c) => c.method === 'POST' && c.path.endsWith('/query/fields/unset'));
    expect(post?.body).toEqual({
      query: { type: 'match', field: 'rating', op: '>', value: 3 },
      name: 'rating',
    });
  });
});
