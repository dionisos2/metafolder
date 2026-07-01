// Unit tests for the finder query builder (panel-shim/finder.js): the
// quick-filter → OSM query composition shared by the list panels.
import { describe, it, expect } from 'vitest';
// @ts-expect-error plain-JS module shared with the panels
import { splitTerms, finderTargets, finderClause, composeQuery } from '/__finder.js';

describe('splitTerms', () => {
  it('splits on whitespace and drops empty runs', () => {
    expect(splitTerms('  scien   fic ')).toEqual(['scien', 'fic']);
    expect(splitTerms('')).toEqual([]);
    expect(splitTerms('   ')).toEqual([]);
  });
});

describe('finderTargets', () => {
  it('maps tree_ref fields to path mode and everything else to direct', () => {
    const typeOf = (f: string) =>
      ({ mfr_path: 'tree_ref', label: 'string' })[f] ?? null; // unknown → null
    expect(finderTargets(['mfr_path', 'label', 'nope'], typeOf)).toEqual([
      { field: 'mfr_path', mode: 'path' },
      { field: 'label', mode: 'direct' },
      { field: 'nope', mode: 'direct' },
    ]);
  });
});

describe('finderClause', () => {
  it('is null when there are no terms', () => {
    expect(finderClause([], [{ field: 'mfr_path', mode: 'path' }])).toBeNull();
  });

  it('uses a single target bare', () => {
    expect(finderClause(['a'], [{ field: 'mfr_path', mode: 'path' }])).toEqual({
      type: 'osm',
      field: 'mfr_path',
      terms: ['a'],
      mode: 'path',
    });
  });

  it('ORs several targets', () => {
    const clause = finderClause(['a', 'b'], [
      { field: 'mfr_path', mode: 'path' },
      { field: 'label', mode: 'direct' },
    ]);
    expect(clause).toEqual({
      type: 'or',
      operands: [
        { type: 'osm', field: 'mfr_path', terms: ['a', 'b'], mode: 'path' },
        { type: 'osm', field: 'label', terms: ['a', 'b'], mode: 'direct' },
      ],
    });
  });
});

describe('composeQuery', () => {
  const clause = { type: 'osm', field: 'label', terms: ['a'], mode: 'direct' };

  it('returns the base unchanged when there is no clause', () => {
    const base = { type: 'eq', field: 'x', value: { type: 'int', value: 1 } };
    expect(composeQuery(base, null)).toBe(base);
  });

  it('returns the clause when the base is match-all (null)', () => {
    expect(composeQuery(null, clause)).toBe(clause);
  });

  it('ANDs the base with the clause', () => {
    const base = { type: 'eq', field: 'x', value: { type: 'int', value: 1 } };
    expect(composeQuery(base, clause)).toEqual({ type: 'and', operands: [base, clause] });
  });
});
