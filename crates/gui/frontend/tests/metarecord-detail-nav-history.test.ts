// metarecord-detail back navigation: following a ref (or picking another
// record in a list) moves `selected_metarecord`; `metarecord:back` returns to
// the record shown before it.

import { describe, expect, test } from 'vitest';
import { createNavHistory } from '../../default-config/panel-types/metarecord-detail/nav-history.js';

const a = { uuid: 'a', repo: 'r' };
const b = { uuid: 'b', repo: 'r' };
const c = { uuid: 'c', repo: 'r' };

describe('createNavHistory', () => {
  test('nothing to go back to before anything was visited', () => {
    const history = createNavHistory();
    expect(history.canGoBack()).toBe(false);
    expect(history.back()).toBeNull();
  });

  test('walks back through the visited records, most recent first', () => {
    const history = createNavHistory();
    history.record(null, a);
    history.record(a, b);
    history.record(b, c);
    expect(history.canGoBack()).toBe(true);
    expect(history.back()).toEqual(b);
    // The back move itself is reported like any other change, and must not be
    // recorded — otherwise back would bounce between the last two records.
    history.record(c, b);
    expect(history.back()).toEqual(a);
    history.record(b, a);
    expect(history.canGoBack()).toBe(false);
    expect(history.back()).toBeNull();
  });

  test('the first selection has nothing before it', () => {
    const history = createNavHistory();
    history.record(null, a);
    expect(history.canGoBack()).toBe(false);
  });

  test('a repeated selection of the same record is not a move', () => {
    const history = createNavHistory();
    history.record(null, a);
    history.record(a, b);
    history.record(b, { uuid: 'b', repo: 'r' });
    expect(history.back()).toEqual(a);
  });

  test('records across repositories keep their repo', () => {
    const other = { uuid: 'a', repo: 'other' };
    const history = createNavHistory();
    history.record(null, a);
    history.record(a, other);
    expect(history.back()).toEqual(a);
  });

  test('a cleared selection is a step you can come back from', () => {
    const history = createNavHistory();
    history.record(null, a);
    history.record(a, null); // metarecord:delete clears the selection
    expect(history.back()).toEqual(a);
  });

  test('a back move the panel refuses does not swallow the next real move', () => {
    // The panel may abort a selection change (an unsaved edit the user keeps),
    // so the move `back()` announced never reaches `record()`.
    const history = createNavHistory();
    history.record(null, a);
    history.record(a, b);
    expect(history.back()).toEqual(a);
    history.cancel();
    history.record(b, c); // a real move again: it must be recorded
    expect(history.back()).toEqual(b);
  });

  test('the trail is bounded: the oldest entries fall off', () => {
    const history = createNavHistory({ limit: 2 });
    history.record(null, a);
    history.record(a, b);
    history.record(b, c);
    history.record(c, { uuid: 'd', repo: 'r' });
    expect(history.back()).toEqual(c);
    history.record({ uuid: 'd', repo: 'r' }, c);
    expect(history.back()).toEqual(b);
    expect(history.canGoBack()).toBe(false);
  });
});
