// Unit tests for the finder query builder (panel-shim/finder.js): the
// quick-filter → OSM query composition shared by the list panels.
import { describe, it, expect } from 'vitest';
import { splitTerms, finderTargets, finderClause, composeQuery, osmMatch } from '/__finder.js';

describe('splitTerms', () => {
  it('splits on whitespace and drops empty runs', () => {
    expect(splitTerms('  scien   fic ')).toEqual(['scien', 'fic']);
    expect(splitTerms('')).toEqual([]);
    expect(splitTerms('   ')).toEqual([]);
  });
});

describe('finderTargets', () => {
  it('honours an explicit `field:mode`, ignoring the catalog', () => {
    // Explicit modes never depend on the (async) field catalog — the robust
    // default so mfr_path is always path mode even before the catalog loads.
    const typeOf = () => 'string'; // would say "direct" for mfr_path
    expect(finderTargets(['mfr_path:path', 'label:direct'], typeOf)).toEqual([
      { field: 'mfr_path', mode: 'path' },
      { field: 'label', mode: 'direct' },
    ]);
  });

  it('auto-detects the mode from the catalog when none is given', () => {
    const typeOf = (f: string) =>
      ({ mfr_path: 'tree_ref', label: 'string' })[f] ?? null; // unknown → null
    expect(finderTargets(['mfr_path', 'label', 'nope'], typeOf)).toEqual([
      { field: 'mfr_path', mode: 'path' },
      { field: 'label', mode: 'direct' },
      { field: 'nope', mode: 'direct' },
    ]);
  });

  it('falls back to direct for an unknown explicit mode', () => {
    expect(finderTargets(['label:weird'], () => null)).toEqual([
      { field: 'label:weird', mode: 'direct' },
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

describe('osmMatch', () => {
  it('matches terms in order as substrings', () => {
    expect(osmMatch('condef', ['con', 'def'])).toBe(true);
    expect(osmMatch('con x def', ['con', 'def'])).toBe(true);
    expect(osmMatch('defcon', ['con', 'def'])).toBe(false);
  });

  it('terms are non-overlapping', () => {
    expect(osmMatch('aa', ['a', 'a'])).toBe(true);
    expect(osmMatch('a', ['a', 'a'])).toBe(false);
  });

  it('is case-insensitive on both sides', () => {
    expect(osmMatch('Repo:List', ['repo', 'LIST'])).toBe(true);
  });

  it('empty terms match everything', () => {
    expect(osmMatch('anything', [])).toBe(true);
    expect(osmMatch('', [])).toBe(true);
  });

  it("has no '/' barrier (plain-text semantics, not path mode)", () => {
    expect(osmMatch('a/b', ['a/b'])).toBe(true);
    expect(osmMatch('dir/file', ['ir/fi'])).toBe(true);
  });
});
