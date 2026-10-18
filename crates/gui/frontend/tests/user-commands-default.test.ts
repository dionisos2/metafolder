// The shipped ~/.config/metafolder/gui/commands.js (doc "User commands").
//
// The default file is executable documentation: `tag:query` is the
// example the spec teaches with, and its *composition* is the whole point —
// the filter lands, the search runs, and the focus is never dragged into the
// simplified query zone on the way. What is pinned here is the pair of
// invocations it composes; what those two do to the panel is
// metarecord-list-search-commands.test.ts.

import { describe, expect, test } from 'vitest';

// An argument as the tests call it: the module's own types are inferred
// from JavaScript, one signature per argument, too narrow for a stub `mf`.
type Arg = { complete: (...args: unknown[]) => unknown };

describe('the shipped user commands', () => {
  test('tag:query inserts the filter with `stay` and runs the search', async () => {
    const mod = await import('../../default-config/commands.js');
    const invocations: string[] = [];
    const mf = {
      invoke: (invocation: string) => {
        invocations.push(invocation);
        return Promise.resolve({ ok: true });
      },
    };

    await mod.default['tag:query'].run(mf as never, 'tag', 'music/jazz');

    expect(invocations).toEqual([
      // Quoted: a tag path may hold spaces, and `stay` trails the text.
      'metarecord-list:insert simplified "#=music/jazz" stay',
      // The Enter nobody presses — it runs the query.
      'metarecord-list:apply simplified',
    ]);
  });

  test('tag:query asks the other fields of the tag model by naming them', async () => {
    const mod = await import('../../default-config/commands.js');
    const invocations: string[] = [];
    const mf = { invoke: (i: string) => (invocations.push(i), Promise.resolve({ ok: true })) };

    await mod.default['tag:query'].run(mf as never, 'negative_tag', 'music/jazz');
    await mod.default['tag:query'].run(mf as never, 'mixed_tag', 'jeux video');

    expect(invocations).toEqual([
      'metarecord-list:insert simplified "negative_tag #=music/jazz" stay',
      'metarecord-list:apply simplified',
      // A path the simplified language must quote cannot ride in a quoted
      // argument (an invocation has no escapes): the text goes unquoted, as
      // the last argument's words, and `stay` is given up for it.
      'metarecord-list:insert simplified mixed_tag #="jeux video"',
      'metarecord-list:apply simplified',
    ]);
    await expect(
      mod.default['tag:query'].run(mf as never, 'tags', 'music'),
    ).rejects.toThrow(/tag, negative_tag or mixed_tag/);
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
    const invocations: string[] = [];
    const mf = { invoke: (i: string) => (invocations.push(i), Promise.resolve({ ok: true })) };
    const jazz = 'a'.repeat(32);

    // The value is the tag entry's uuid, which a ref takes as itself.
    await mod.default['tag:remove'].run(mf as never, 'negative_tag', jazz);
    // A path typed rather than picked goes through as typed: the value is
    // the last argument, so its words need no quotes.
    await mod.default['tag:remove'].run(mf as never, 'tag', 'jeux video');

    expect(invocations).toEqual([
      `metarecord:field remove negative_tag ${jazz}`,
      'metarecord:field remove tag jeux video',
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

    const invocations: string[] = [];
    const mf = { invoke: (i: string) => (invocations.push(i), Promise.resolve({ ok: true })) };
    await own.default['user:rate'].run(mf as never, '4');
    expect(invocations).toEqual(['metarecord:bulk selection set rating int 4']);
  });

  test('commands.js exports its helpers for the user file to import', async () => {
    const shipped = await import('../../default-config/commands.js');
    for (const helper of ['selectedMetarecord', 'tagPaths', 'tagKindArg', 'requireTagKind']) {
      expect(typeof (shipped as Record<string, unknown>)[helper]).toBe('function');
    }
  });

  test('the two shipped files name no command twice', async () => {
    const { mergeCommandModules } = await import('../src/lib/commands');
    const own = await import('../../default-config/user-commands.js');
    const shipped = await import('../../default-config/commands.js');
    expect(mergeCommandModules(shipped.default, own.default).shadowed).toEqual([]);
  });
});
