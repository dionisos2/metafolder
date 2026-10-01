// The shipped ~/.config/metafolder/gui/commands.js (doc "User commands").
//
// The default file is executable documentation: `user:tag-query` is the
// example the spec teaches with, and its *composition* is the whole point —
// the filter lands, the search runs, and the focus is never dragged into the
// simplified query zone on the way. What is pinned here is the pair of
// invocations it composes; what those two do to the panel is
// metarecord-list-search-commands.test.ts.

import { describe, expect, test } from 'vitest';

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

    await mod.default['user:tag-query'].run(mf as never, 'music/jazz');

    expect(invocations).toEqual([
      // Quoted: a tag path may hold spaces, and `stay` trails the text.
      'metarecord-list:insert simplified "#=music/jazz" stay',
      // The Enter nobody presses — it runs the query.
      'metarecord-list:apply simplified',
    ]);
  });
});
