// The completion candidate rules the system applies to *any* command
// (spec-gui "Completion views"): a plain string is its own label and value;
// two candidates that read the same are both suffixed with their values, so
// every listed row is addressable and names exactly one thing; a page says
// whether it is the whole set; a bound view memoizes its last page, because
// the eager first page and the prompt's own first load are one and the same
// call.
import { describe, expect, test } from 'vitest';
import { bindView, completionLabels, completionPage } from '../src/lib/completions';

describe('completionLabels', () => {
  test('a plain string is its own label and value', () => {
    expect(completionLabels(['jazz', 'rock'])).toEqual([
      { label: 'jazz', value: 'jazz' },
      { label: 'rock', value: 'rock' },
    ]);
  });

  test('identical labels are suffixed with their values — the duplicate-label rule', () => {
    // Two metarecords named alike: the uuid is what keeps their rows apart.
    expect(
      completionLabels([
        { label: 'musique', value: 'uuid-1' },
        { label: 'musique', value: 'uuid-2' },
      ]),
    ).toEqual([
      { label: 'musique (uuid-1)', value: 'uuid-1' },
      { label: 'musique (uuid-2)', value: 'uuid-2' },
    ]);
  });

  test('the rule is about labels: a string reading like a couple is a duplicate too', () => {
    expect(
      completionLabels([
        { label: 'musique', value: 'uuid-1' },
        'musique',
      ]),
    ).toEqual([
      { label: 'musique (uuid-1)', value: 'uuid-1' },
      { label: 'musique (musique)', value: 'musique' },
    ]);
  });

  test('a lone label is never suffixed', () => {
    expect(completionLabels([{ label: 'musique', value: 'uuid-1' }, 'rock'])).toEqual([
      { label: 'musique', value: 'uuid-1' },
      { label: 'rock', value: 'rock' },
    ]);
  });

  test('exact duplicates collapse (same label, same value)', () => {
    expect(
      completionLabels([
        'a',
        'a',
        { label: 'b', value: 'v' },
        { label: 'b', value: 'v' },
      ]),
    ).toEqual([
      { label: 'a', value: 'a' },
      { label: 'b', value: 'v' },
    ]);
  });

  test('no two listed rows ever read the same', () => {
    const labels = completionLabels([
      { label: 'x', value: '1' },
      { label: 'x', value: '2' },
      { label: 'x', value: '3' },
      'x',
    ]).map((item) => item.label);
    expect(new Set(labels).size).toBe(labels.length);
  });
});

describe('completionPage', () => {
  test('a bare list is a complete page', () => {
    expect(completionPage(['a'])).toEqual({ items: ['a'] });
  });

  test('a page keeps its `more`', () => {
    expect(completionPage({ items: ['a'], more: true })).toEqual({ items: ['a'], more: true });
    expect(completionPage({ items: [] })).toEqual({ items: [] });
  });
});

describe('bindView', () => {
  test('binds `prior` and passes the typed text through', async () => {
    const view = bindView(
      {
        title: 'path · name',
        items: (partial, prior) => ({ items: [`${prior.join('/')}|${partial}`] }),
      },
      ['edit', 'tag'],
    );
    expect(view.title).toBe('path · name');
    expect(await view.items('mu')).toEqual({ items: ['edit/tag|mu'] });
  });

  test('memoizes the last page — one query per typed text, not per render', async () => {
    let calls = 0;
    const view = bindView(
      { items: (partial) => (calls += 1, { items: [partial] }) },
      [],
    );
    const first = view.items('mu');
    expect(await view.items('mu')).toEqual({ items: ['mu'] });
    expect(await first).toEqual({ items: ['mu'] });
    expect(calls).toBe(1);
    await view.items('mus'); // a new typed text is a new query
    expect(calls).toBe(2);
    await view.items('mu'); // and back is one again: only the last is kept
    expect(calls).toBe(3);
  });
});
