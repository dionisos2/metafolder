// The shipped ~/.config/metafolder/gui/commands.js (doc "User commands").
//
// The default file is executable documentation: `tag:query` is the example the
// spec teaches with. What is pinned here is the normal query it writes; the list
// follows that variable as it does any command's query (doc "Cross-panel
// selection").

import { describe, expect, test, vi } from 'vitest';

// An argument as the tests call it: the module's own types are inferred
// from JavaScript, one signature per argument, too narrow for a stub `mf`.
type Arg = { complete: (...args: unknown[]) => unknown };

describe('the shipped user commands', () => {
  // A stub whose workspace holds the query variables, what the list follows.
  function queryStub(normal = '') {
    const vars = new Map<string, unknown>([['metarecord-list:normal-query', normal]]);
    const writes: unknown[][] = [];
    const errors: string[] = [];
    const mf = {
      workspace: {
        get: (key: string) => Promise.resolve(vars.get(key) ?? null),
        set: (key: string, value: unknown) => {
          writes.push([key, value]);
          vars.set(key, value);
          return Promise.resolve();
        },
      },
      statusBar: {
        error: (message: string) => {
          errors.push(message);
          return Promise.resolve();
        },
      },
    };
    return { mf, writes, errors, normal: () => vars.get('metarecord-list:normal-query') };
  }

  test('tag:query writes the filter in the normal query, without the schema test', async () => {
    const mod = await import('../../default-config/commands.js');
    const { mf, writes } = queryStub();

    await mod.default['tag:query'].run(mf as never, 'tag', 'music/jazz');

    // The list follows this variable, and runs what it now holds.
    expect(writes).toEqual([['metarecord-list:normal-query', 'tag -> (path =>* "music/jazz")']]);
  });

  test('tag:query asks the other fields of the tag model by naming them', async () => {
    const mod = await import('../../default-config/commands.js');
    const { mf, normal } = queryStub();

    await mod.default['tag:query'].run(mf as never, 'negative_tag', 'music/jazz');
    await mod.default['tag:query'].run(mf as never, 'mixed_tag', 'jeux "video"');

    expect(normal()).toBe(
      'negative_tag -> (path =>* "music/jazz") AND mixed_tag -> (path =>* "jeux \\"video\\"")',
    );
    await expect(
      mod.default['tag:query'].run(mf as never, 'tags', 'music'),
    ).rejects.toThrow(/tag, negative_tag or mixed_tag/);
  });

  test('tag:query adds a tag once: the second time writes nothing', async () => {
    const mod = await import('../../default-config/commands.js');
    const { mf, writes } = queryStub();

    await mod.default['tag:query'].run(mf as never, 'tag', 'music');
    await mod.default['tag:query'].run(mf as never, 'tag', 'sport');
    await mod.default['tag:query'].run(mf as never, 'tag', 'music');

    expect(writes.length).toBe(2);
    expect(writes[1]?.[1]).toBe('tag -> (path =>* "music") AND tag -> (path =>* "sport")');
  });

  test('tag:query treats the same tag of another field as another filter', async () => {
    const mod = await import('../../default-config/commands.js');
    const { mf, normal } = queryStub('tag -> (path =>* "music")');

    await mod.default['tag:query'].run(mf as never, 'negative_tag', 'music');

    expect(normal()).toBe('tag -> (path =>* "music") AND negative_tag -> (path =>* "music")');
  });

  test('tag:query narrows a query holding an OR, not its last branch', async () => {
    const mod = await import('../../default-config/commands.js');
    const { mf, normal } = queryStub('a = 1 OR b = 2');

    await mod.default['tag:query'].run(mf as never, 'tag', 'music');
    await mod.default['tag:query'].run(mf as never, 'tag', 'sport');

    expect(normal()).toBe(
      '(a = 1 OR b = 2) AND tag -> (path =>* "music") AND tag -> (path =>* "sport")',
    );
  });

  test('a filter only counts as present when it is a whole term of the AND', async () => {
    const mod = await import('../../default-config/commands.js');
    const clause = 'tag -> (path =>* "music")';
    for (const [query, expected] of [
      // Under an OR or a NOT, or quoted: the query does not yet require the tag.
      [`a = 1 OR ${clause}`, `(a = 1 OR ${clause}) AND ${clause}`],
      [`NOT ${clause}`, `NOT ${clause} AND ${clause}`],
      [`(${clause} OR a = 1) AND b = 2`, `(${clause} OR a = 1) AND b = 2 AND ${clause}`],
      // AND and OR inside a string or a word split nothing.
      ['name = "x OR y" AND ORDER = 1', `name = "x OR y" AND ORDER = 1 AND ${clause}`],
      ['name = "a\\" AND b"', `name = "a\\" AND b" AND ${clause}`],
    ] as const) {
      const { mf, normal } = queryStub(query);
      await mod.default['tag:query'].run(mf as never, 'tag', 'music');
      expect(normal()).toBe(expected);
    }
  });

  test('tag:unquery takes the filter off and keeps the rest', async () => {
    const mod = await import('../../default-config/commands.js');
    const { mf, normal } = queryStub(
      'mfr_path -> "/m" AND tag -> (path =>* "music") AND tag -> (path =>* "sport")',
    );

    await mod.default['tag:unquery'].run(mf as never, 'tag', 'music');
    expect(normal()).toBe('mfr_path -> "/m" AND tag -> (path =>* "sport")');

    await mod.default['tag:unquery'].run(mf as never, 'tag', 'sport');
    await expect(
      mod.default['tag:unquery'].run(mf as never, 'tags', 'music'),
    ).rejects.toThrow(/tag, negative_tag or mixed_tag/);
    expect(normal()).toBe('mfr_path -> "/m"');
  });

  test('tag:unquery says so when the filter is not in the query', async () => {
    const mod = await import('../../default-config/commands.js');
    const { mf, writes, errors } = queryStub('negative_tag -> (path =>* "music")');

    await mod.default['tag:unquery'].run(mf as never, 'tag', 'music');

    expect(writes).toEqual([]);
    expect(errors).toEqual(['tag "music" is not in the query']);
  });

  test('tag:unquery completes over the tags of that field in the query', async () => {
    const mod = await import('../../default-config/commands.js');
    const { mf } = queryStub(
      'tag -> (path =>* "music") AND negative_tag -> (path =>* "sport") AND tag -> (path =>* "jeux \\"video\\"") AND tag -> (path = "exact")',
    );
    const [kind, tag] = mod.default['tag:unquery'].args as unknown as Arg[];

    expect(await kind.complete(mf)).toEqual(['tag', 'negative_tag', 'mixed_tag']);
    expect(await tag.complete(mf, '', ['tag'])).toEqual(['music', 'jeux "video"']);
    expect(await tag.complete(mf, '', ['negative_tag'])).toEqual(['sport']);
  });

  test('tag:query completes the kind, then every tag path', async () => {
    const mod = await import('../../default-config/commands.js');
    const calls: unknown[][] = [];
    const mf = {
      workspace: { get: (k: string) => Promise.resolve(k === 'active_repo' ? 'r1' : null) },
      daemon: {
        call: (...args: unknown[]) => {
          calls.push(args);
          // A tag hanging under two parents has two paths.
          return Promise.resolve({ a: ['music/jazz'], b: ['sport/run/marathon', 'event/marathon'] });
        },
      },
    };
    const [kind, tag] = mod.default['tag:query'].args as unknown as Arg[];

    expect(await kind.complete(mf)).toEqual(['tag', 'negative_tag', 'mixed_tag']);
    expect(await tag.complete(mf, '', ['tag'])).toEqual([
      'event/marathon',
      'music/jazz',
      'sport/run/marathon',
    ]);
    // The paths of the tag entries — not of the `tag` field, which is a ref.
    expect(calls).toEqual([
      [
        'POST',
        '/repos/r1/query/fields/resolve-tree',
        {
          query: { type: 'eq', field: 'mf_schema', value: { type: 'string', value: 'tag' } },
          field: 'path',
        },
      ],
    ]);
  });

  test('tag:add answers about the tag on the selected metarecord, the kind naming the answer', async () => {
    const mod = await import('../../default-config/commands.js');
    const answers: unknown[][] = [];
    const statuses: string[] = [];
    const mf = {
      workspace: {
        get: (k: string) =>
          Promise.resolve(k === 'selected_metarecord' ? { uuid: 'u1', repo: 'r1' } : null),
      },
      tag: {
        answer: (...a: unknown[]) => (
          answers.push(a), Promise.resolve({ written: 1, message: `Tag: ${String(a[3])} done` })
        ),
      },
      statusBar: { message: (t: string) => (statuses.push(t), Promise.resolve()) },
      changes: { sync: vi.fn(() => Promise.resolve()) },
    };

    await mod.default['tag:add'].run(mf as never, 'tag', 'music/jazz');
    await mod.default['tag:add'].run(mf as never, 'negative_tag', 'jeux video');
    await mod.default['tag:add'].run(mf as never, 'mixed_tag', 'music');

    expect(answers).toEqual([
      ['r1', 'u1', 'add', 'music/jazz'],
      ['r1', 'u1', 'deny', 'jeux video'],
      ['r1', 'u1', 'mixed', 'music'],
    ]);
    expect(statuses).toEqual(['Tag: music/jazz done', 'Tag: jeux video done', 'Tag: music done']);
    // The panels showing the record hear of the answer now, not at the next poll.
    expect(mf.changes.sync.mock.calls).toEqual([['r1'], ['r1'], ['r1']]);
    await expect(mod.default['tag:add'].run(mf as never, 'tags', 'x')).rejects.toThrow(
      /tag, negative_tag or mixed_tag/,
    );
  });

  test('tag:add with nothing selected says so and answers nothing', async () => {
    const mod = await import('../../default-config/commands.js');
    const answers: unknown[] = [];
    const errors: string[] = [];
    const mf = {
      workspace: { get: () => Promise.resolve(null) },
      tag: { answer: (...a: unknown[]) => (answers.push(a), Promise.resolve({})) },
      statusBar: { error: (t: string) => (errors.push(t), Promise.resolve()) },
    };

    await mod.default['tag:add'].run(mf as never, 'tag', 'music');

    expect(answers).toEqual([]);
    expect(errors).toEqual(['no metarecord is selected']);
  });

  test('tag:add completes the kind, then every tag path', async () => {
    const mod = await import('../../default-config/commands.js');
    const mf = {
      workspace: { get: (k: string) => Promise.resolve(k === 'active_repo' ? 'r1' : null) },
      daemon: { call: () => Promise.resolve({ a: ['music/jazz'], b: ['music'] }) },
    };
    const [kind, tag] = mod.default['tag:add'].args as unknown as Arg[];

    expect(await kind.complete(mf)).toEqual(['tag', 'negative_tag', 'mixed_tag']);
    expect(await tag.complete(mf, '', ['tag'])).toEqual(['music', 'music/jazz']);
  });

  test('tag:remove takes a tag off the selected metarecord', async () => {
    const mod = await import('../../default-config/commands.js');
    const invocations: unknown[] = [];
    const mf = { invoke: (i: unknown) => (invocations.push(i), Promise.resolve({ ok: true })) };
    const jazz = 'a'.repeat(32);

    // The value is the tag entry's uuid, which a ref takes as itself.
    await mod.default['tag:remove'].run(mf as never, 'negative_tag', jazz);
    // A path typed rather than picked goes through as typed: one element.
    await mod.default['tag:remove'].run(mf as never, 'tag', 'jeux video');

    expect(invocations).toEqual([
      ['metarecord:field', 'remove', 'negative_tag', jazz],
      ['metarecord:field', 'remove', 'tag', 'jeux video'],
    ]);
  });

  test("tag:remove completes over the selected metarecord's tags of that kind", async () => {
    const mod = await import('../../default-config/commands.js');
    const jazz = 'a'.repeat(32);
    const marathon = 'b'.repeat(32);
    const rock = 'c'.repeat(32);
    const ref = (name: string, uuid: string) => ({ name, value: { type: 'ref', value: uuid } });
    const resolved: unknown[][] = [];
    const mf = {
      workspace: {
        get: (k: string) =>
          Promise.resolve(k === 'selected_metarecord' ? { uuid: 'm1', repo: 'r1' } : null),
      },
      daemon: {
        metarecords: () =>
          Promise.resolve(
            new Map([
              [
                'm1',
                {
                  uuid: 'm1',
                  fields: [ref('tag', rock), ref('negative_tag', jazz), ref('negative_tag', marathon)],
                },
              ],
            ]),
          ),
        treePaths: (...args: unknown[]) => {
          resolved.push(args);
          return Promise.resolve({
            [jazz]: ['music/jazz'],
            [marathon]: ['sport/run/marathon', 'event/marathon'],
          });
        },
      },
    };
    const [kind, tag] = mod.default['tag:remove'].args as unknown as Arg[];

    expect(await kind.complete(mf)).toEqual(['tag', 'negative_tag', 'mixed_tag']);
    expect(await tag.complete(mf, '', ['negative_tag'])).toEqual([
      { label: 'event/marathon', value: marathon },
      { label: 'music/jazz', value: jazz },
      { label: 'sport/run/marathon', value: marathon },
    ]);
    expect(resolved).toEqual([['r1', 'path', [jazz, marathon]]]);
  });
});

describe('the shipped user-commands.js', () => {
  test('holds the example user:rate, which commands.js no longer does', async () => {
    const own = await import('../../default-config/user-commands.js');
    const shipped = await import('../../default-config/commands.js');
    expect(Object.keys(own.default)).toEqual(['user:rate']);
    expect(Object.keys(shipped.default)).not.toContain('user:rate');

    const invocations: unknown[] = [];
    const mf = { invoke: (i: unknown) => (invocations.push(i), Promise.resolve({ ok: true })) };
    await own.default['user:rate'].run(mf as never, '4');
    expect(invocations).toEqual([['metarecord:bulk', 'selection', 'set', 'rating', 'int', '4']]);
  });

  test('commands.js exports its helpers for the user file to import', async () => {
    const shipped = await import('../../default-config/commands.js');
    for (const helper of ['selectedMetarecord', 'tagPaths', 'tagKindArg', 'requireTagKind']) {
      expect(typeof (shipped as Record<string, unknown>)[helper]).toBe('function');
    }
  });

  // A pre-fill is a default answer, and defaults belong to the keybindings,
  // which name every argument ("g r" = `recent other time`): typed by hand, a
  // command asks with an empty line. Only an edit of an existing value (a
  // field's value, a name) starts from that value.
  test('no shipped command pre-fills a choice when called explicitly', async () => {
    const shipped = await import('../../default-config/commands.js');
    const prefilled: string[] = [];
    for (const [name, spec] of Object.entries(shipped.default)) {
      for (const arg of ((spec as { args?: { name: string; initial?: unknown }[] }).args ?? [])) {
        if (arg.initial !== undefined) prefilled.push(`${name} ${arg.name}`);
      }
    }
    expect(prefilled).toEqual([]);
    expect(shipped.whereArg()).not.toHaveProperty('initial');
    expect(shipped.tagKindArg('Which tags?')).not.toHaveProperty('initial');
  });

  test('the two shipped files name no command twice', async () => {
    const { mergeCommandModules } = await import('../src/lib/commands');
    const own = await import('../../default-config/user-commands.js');
    const shipped = await import('../../default-config/commands.js');
    expect(mergeCommandModules(shipped.default, own.default).shadowed).toEqual([]);
  });
});
