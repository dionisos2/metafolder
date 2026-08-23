// Ref-value completion/resolution helper (metarecord-detail/ref-completion.js).
// A `ref` field can be given a completion seed (config.toml
// `[ref-completion-seeds]`): a tree_ref field name whose paths seed the value
// completion, and against which a typed path is resolved back to the target
// metarecord uuid — the symmetric of how tree_ref values are resolved.

import { describe, expect, test, vi } from 'vitest';
import {
  HEX32,
  completionSourceField,
  resolveRefValue,
} from '../../default-config/panel-types/metarecord-detail/ref-completion.js';

const UUID = 'a'.repeat(32);

describe('HEX32', () => {
  test('matches a 32-char lowercase hex uuid', () => {
    expect(HEX32.test(UUID)).toBe(true);
    expect(HEX32.test('0123456789abcdef0123456789abcdef')).toBe(true);
  });
  test('rejects a path, wrong length or upper case', () => {
    expect(HEX32.test('animals/cats')).toBe(false);
    expect(HEX32.test('a'.repeat(31))).toBe(false);
    expect(HEX32.test('A'.repeat(32))).toBe(false);
  });
});

describe('completionSourceField', () => {
  test('a tree_ref field seeds completion from its own forest', () => {
    expect(completionSourceField('tree_ref', 'mfr_path', null)).toBe('mfr_path');
    // The seed argument is ignored for tree_ref (it has no completion seed).
    expect(completionSourceField('tree_ref', 'mfr_path', 'other')).toBe('mfr_path');
  });
  test('a ref field with a seed sources completion from the seed field', () => {
    expect(completionSourceField('ref', 'tag', 'path')).toBe('path');
  });
  test('a ref field without a seed offers no completion', () => {
    expect(completionSourceField('ref', 'tag', null)).toBe(null);
  });
  test('other types offer no completion', () => {
    expect(completionSourceField('string', 'title', null)).toBe(null);
    expect(completionSourceField('int', 'rating', 'path')).toBe(null);
  });
});

describe('resolveRefValue', () => {
  test('a 32-hex value is taken as the uuid directly, without resolving', async () => {
    const resolvePath = vi.fn();
    expect(await resolveRefValue(`  ${UUID}  `, 'path', resolvePath)).toBe(UUID);
    expect(resolvePath).not.toHaveBeenCalled();
  });

  test('a non-hex value with a seed is resolved to a uuid through the seed field', async () => {
    const resolvePath = vi.fn(async (field: string, path: string) =>
      field === 'path' && path === 'animals/cats' ? UUID : null,
    );
    expect(await resolveRefValue(' animals/cats ', 'path', resolvePath)).toBe(UUID);
    expect(resolvePath).toHaveBeenCalledWith('path', 'animals/cats');
  });

  test('a non-hex value that resolves to nothing is a hard error', async () => {
    const resolvePath = vi.fn(async () => null);
    await expect(resolveRefValue('animals/dogs', 'path', resolvePath)).rejects.toThrow(
      /animals\/dogs/,
    );
  });

  test('without a seed, a non-hex value is passed through untouched (raw uuid)', async () => {
    const resolvePath = vi.fn();
    expect(await resolveRefValue('  whatever  ', null, resolvePath)).toBe('whatever');
    expect(resolvePath).not.toHaveBeenCalled();
  });
});
