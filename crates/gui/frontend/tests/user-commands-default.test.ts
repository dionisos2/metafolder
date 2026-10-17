// The shipped ~/.config/metafolder/gui/commands.js (doc "User commands").
//
// The default file is executable documentation: `user:tag-query` is the
// example the spec teaches with, and its *composition* is the whole point —
// the filter lands, the search runs, and the focus is never dragged into the
// simplified query zone on the way. What is pinned here is the pair of
// invocations it composes; what those two do to the panel is
// metarecord-list-search-commands.test.ts.

import { describe, expect, test } from 'vitest';

// An argument as the tests call it: the module's own types are inferred
// from JavaScript, one signature per argument, too narrow for a stub `mf`.
type Arg = { complete: (...args: unknown[]) => Promise<unknown> | unknown };

describe('the shipped user commands', () => {
  test('user:tag-query inserts the filter with `stay` and runs the search', async () => {
    const mod = await import('../../default-config/commands.js');
    const invocations: string[] = [];
    const mf = {
      invoke: (invocation: string) => {
        invocations.push(invocation);
        return Promise.resolve({ ok: true });
      },
    };

    await mod.default['user:tag-query'].run(mf as never, 'tag', 'music/jazz');

    expect(invocations).toEqual([
      // Quoted: a tag path may hold spaces, and `stay` trails the text.
      'metarecord-list:insert simplified "#=music/jazz" stay',
      // The Enter nobody presses — it runs the query.
      'metarecord-list:apply simplified',
    ]);
  });

  test('user:tag-query asks the other fields of the tag model by naming them', async () => {
    const mod = await import('../../default-config/commands.js');
    const invocations: string[] = [];
    const mf = { invoke: (i: string) => (invocations.push(i), Promise.resolve({ ok: true })) };

    await mod.default['user:tag-query'].run(mf as never, 'negative_tag', 'music/jazz');
    await mod.default['user:tag-query'].run(mf as never, 'mixed_tag', 'jeux video');

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
      mod.default['user:tag-query'].run(mf as never, 'tags', 'music'),
    ).rejects.toThrow(/tag, negative_tag or mixed_tag/);
  });

  test('user:tag-query completes the kind, then every tag path', async () => {
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
    const [kind, tag] = mod.default['user:tag-query'].args as unknown as Arg[];

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

  test('user:tag-remove takes a tag off the selected metarecord', async () => {
    const mod = await import('../../default-config/commands.js');
    const invocations: string[] = [];
    const mf = { invoke: (i: string) => (invocations.push(i), Promise.resolve({ ok: true })) };
    const jazz = 'a'.repeat(32);

    // The value is the tag entry's uuid, which a ref takes as itself.
    await mod.default['user:tag-remove'].run(mf as never, 'negative_tag', jazz);
    // A path typed rather than picked goes through as typed: the value is
    // the last argument, so its words need no quotes.
    await mod.default['user:tag-remove'].run(mf as never, 'tag', 'jeux video');

    expect(invocations).toEqual([
      `metarecord:field remove negative_tag ${jazz}`,
      'metarecord:field remove tag jeux video',
    ]);
  });

  test("user:tag-remove completes over the selected metarecord's tags of that kind", async () => {
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
    const [kind, tag] = mod.default['user:tag-remove'].args as unknown as Arg[];

    expect(await kind.complete(mf)).toEqual(['tag', 'negative_tag', 'mixed_tag']);
    expect(await tag.complete(mf, '', ['negative_tag'])).toEqual([
      { label: 'event/marathon', value: marathon },
      { label: 'music/jazz', value: jazz },
      { label: 'sport/run/marathon', value: marathon },
    ]);
    expect(resolved).toEqual([['r1', 'path', [jazz, marathon]]]);
  });
});
