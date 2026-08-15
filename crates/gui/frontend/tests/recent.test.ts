// The recently-viewed picker's pure display helpers (lib/recent.ts): how one
// recent metarecord becomes a single candidate line "<mfr_path> — <label> —
// <name>" for the command input's ordered-substring filter.

import { describe, expect, test } from 'vitest';
import { firstFieldText, recentLine } from '../src/lib/recent';

/** @param entries name → Value */
function rec(uuid: string, entries: [string, Metafolder.Value][]): Metafolder.Metarecord {
  return { uuid, fields: entries.map(([name, value]) => ({ name, value })) };
}

const str = (value: string): Metafolder.Value => ({ type: 'string', value });

describe('firstFieldText', () => {
  test('returns the first matching field rendered as text', () => {
    const m = rec('u', [['label', str('Blue')]]);
    expect(firstFieldText(m, 'label')).toBe('Blue');
  });

  test('picks the first row of a multi-map field', () => {
    const m = rec('u', [['tag', str('a')], ['tag', str('b')]]);
    expect(firstFieldText(m, 'tag')).toBe('a');
  });

  test('absent field is the empty string', () => {
    expect(firstFieldText(rec('u', []), 'label')).toBe('');
  });

  test('a nothing value is the empty string (explicit absence)', () => {
    expect(firstFieldText(rec('u', [['label', { type: 'nothing' }]]), 'label')).toBe('');
  });

  test('non-string scalars stringify', () => {
    expect(firstFieldText(rec('u', [['rating', { type: 'int', value: 5 }]]), 'rating')).toBe('5');
  });
});

describe('recentLine', () => {
  test('joins path, label and name with an em dash', () => {
    const m = rec('u', [['label', str('Blue')], ['name', str('jazz.mp3')]]);
    expect(recentLine(m, 'music/jazz.mp3')).toBe('music/jazz.mp3 — Blue — jazz.mp3');
  });

  test('drops the missing parts (no label)', () => {
    const m = rec('u', [['name', str('jazz.mp3')]]);
    expect(recentLine(m, 'music/jazz.mp3')).toBe('music/jazz.mp3 — jazz.mp3');
  });

  test('a record with no path/label/name falls back to the uuid', () => {
    expect(recentLine(rec('deadbeef', []), '')).toBe('deadbeef');
  });

  test('tolerates a missing metarecord (uuid fallback)', () => {
    expect(recentLine(undefined, '', 'the-uuid')).toBe('the-uuid');
  });
});
