// Pure logic of the bash input's Tab completion (lib/bash.ts): longest
// common prefix and candidate insertion. The candidates themselves come
// from the Rust `bash_complete` command, exercised in cargo tests.

import { describe, expect, test } from 'vitest';
import { commonPrefix, insertCandidate } from '../src/lib/bash';

describe('commonPrefix', () => {
  test('longest shared prefix of all candidates', () => {
    expect(commonPrefix(['alpha.txt', 'alpha2.txt', 'alphadir/'])).toBe('alpha');
  });

  test('single candidate is its own prefix', () => {
    expect(commonPrefix(['unique'])).toBe('unique');
  });

  test('no shared prefix yields the empty string', () => {
    expect(commonPrefix(['one', 'two'])).toBe('');
  });

  test('empty list yields the empty string', () => {
    expect(commonPrefix([])).toBe('');
  });
});

describe('insertCandidate', () => {
  test('replaces the completed word ending at the cursor', () => {
    const result = insertCandidate('cat alph', 8, 'alph', 'alpha.txt', true);
    expect(result.text).toBe('cat alpha.txt ');
    expect(result.cursor).toBe(14);
  });

  test('a final completion gets a trailing space', () => {
    const result = insertCandidate('ech', 3, 'ech', 'echo', true);
    expect(result.text).toBe('echo ');
    expect(result.cursor).toBe(5);
  });

  test('a directory completion gets no trailing space (bash behaviour)', () => {
    const result = insertCandidate('ls sr', 5, 'sr', 'src/', true);
    expect(result.text).toBe('ls src/');
    expect(result.cursor).toBe(7);
  });

  test('a partial (common-prefix) completion gets no trailing space', () => {
    const result = insertCandidate('cat al', 6, 'al', 'alpha', false);
    expect(result.text).toBe('cat alpha');
    expect(result.cursor).toBe(9);
  });

  test('text after the cursor is preserved', () => {
    const result = insertCandidate('cat al | wc -l', 6, 'al', 'alpha.txt', true);
    expect(result.text).toBe('cat alpha.txt  | wc -l');
    expect(result.cursor).toBe(14);
  });

  test('an empty word inserts at the cursor', () => {
    const result = insertCandidate('ls ', 3, '', 'file.txt', true);
    expect(result.text).toBe('ls file.txt ');
    expect(result.cursor).toBe(12);
  });
});
