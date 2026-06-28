// Placeholder substitution for `!` shell commands (lib/placeholders.ts):
// `%u`/`%v`/`%<field>`/`%<field>:path` expand to data from the selected
// metarecord. Parsing and substitution are pure; the async orchestrator is
// driven with stub deps. spec-gui "Command input".

import { describe, expect, test, vi } from 'vitest';
import {
  type ExpandDeps,
  expandShellPlaceholders,
  parsePlaceholders,
  substitute,
} from '../src/lib/placeholders';

describe('parsePlaceholders', () => {
  test('plain text has no placeholders', () => {
    expect(parsePlaceholders('ls -la /tmp')).toEqual([{ lit: 'ls -la /tmp' }]);
  });

  test('uuid and version shorthands', () => {
    expect(parsePlaceholders('echo %u %v')).toEqual([
      { lit: 'echo ' },
      { ph: { name: 'u', path: false } },
      { lit: ' ' },
      { ph: { name: 'v', path: false } },
    ]);
  });

  test('field value and field path', () => {
    expect(parsePlaceholders('open %mfr_path:path then %tag')).toEqual([
      { lit: 'open ' },
      { ph: { name: 'mfr_path', path: true } },
      { lit: ' then ' },
      { ph: { name: 'tag', path: false } },
    ]);
  });

  test('%% is a literal percent', () => {
    expect(parsePlaceholders('100%% done %u')).toEqual([
      { lit: '100' },
      { lit: '%' },
      { lit: ' done ' },
      { ph: { name: 'u', path: false } },
    ]);
  });

  test('a bare percent (not a placeholder) stays literal', () => {
    expect(parsePlaceholders('50% off')).toEqual([{ lit: '50% off' }]);
    expect(parsePlaceholders('trailing %')).toEqual([{ lit: 'trailing %' }]);
  });
});

describe('substitute', () => {
  const segs = parsePlaceholders('echo %u %mfr_path:path');

  test('shell-quotes every substituted value', () => {
    const resolved = new Map([
      ['u', 'abc-123'],
      ['mfr_path:path', '/home/My Files/a.txt'],
    ]);
    expect(substitute(segs, resolved)).toEqual({
      ok: true,
      value: "echo 'abc-123' '/home/My Files/a.txt'",
    });
  });

  test('escapes embedded single quotes', () => {
    const resolved = new Map([
      ['u', "it's"],
      ['mfr_path:path', 'x'],
    ]);
    expect(substitute(segs, resolved)).toEqual({
      ok: true,
      value: "echo 'it'\\''s' 'x'",
    });
  });

  test('a missing resolution aborts', () => {
    const resolved = new Map([['u', 'abc']]);
    const out = substitute(segs, resolved);
    expect(out.ok).toBe(false);
  });
});

// ── Orchestrator ───────────────────────────────────────────────────────

const SELECTED = { uuid: 'uuid-1', repo: 'repo-1' };

function deps(over: Partial<ExpandDeps> = {}): ExpandDeps {
  return {
    selected: async () => SELECTED,
    metarecord: async () => ({
      version: 7,
      fields: [
        { name: 'tag', value: { type: 'string', value: 'jazz' } },
        { name: 'rating', value: { type: 'int', value: 5 } },
        { name: 'mfr_path', value: { type: 'tree_ref', value: { parent: null, name: 'a.txt' } } },
      ],
    }),
    treePaths: async () => ['/music/a.txt'],
    ...over,
  };
}

describe('expandShellPlaceholders', () => {
  test('no placeholders: returns input untouched, no selection needed', async () => {
    const selected = vi.fn(async () => null);
    const out = await expandShellPlaceholders('ls -la', deps({ selected }));
    expect(out).toEqual({ ok: true, value: 'ls -la' });
    expect(selected).not.toHaveBeenCalled();
  });

  test('expands %u and %v', async () => {
    const out = await expandShellPlaceholders('echo %u %v', deps());
    expect(out).toEqual({ ok: true, value: "echo 'uuid-1' '7'" });
  });

  test('expands a scalar field value', async () => {
    const out = await expandShellPlaceholders('echo %tag %rating', deps());
    expect(out).toEqual({ ok: true, value: "echo 'jazz' '5'" });
  });

  test('expands a TreeRef field to its resolved path', async () => {
    const treePaths = vi.fn(async () => ['/music/a.txt']);
    const out = await expandShellPlaceholders('play %mfr_path:path', deps({ treePaths }));
    expect(out).toEqual({ ok: true, value: "play '/music/a.txt'" });
    expect(treePaths).toHaveBeenCalledWith('repo-1', 'uuid-1', 'mfr_path');
  });

  test('fetches the metarecord at most once', async () => {
    const metarecord = vi.fn(deps().metarecord);
    await expandShellPlaceholders('echo %v %tag %rating', deps({ metarecord }));
    expect(metarecord).toHaveBeenCalledTimes(1);
  });

  test('aborts when nothing is selected', async () => {
    const out = await expandShellPlaceholders('echo %u', deps({ selected: async () => null }));
    expect(out.ok).toBe(false);
    if (!out.ok) expect(out.error).toMatch(/select/i);
  });

  test('aborts on an absent field', async () => {
    const out = await expandShellPlaceholders('echo %missing', deps());
    expect(out.ok).toBe(false);
    if (!out.ok) expect(out.error).toMatch(/missing/);
  });

  test('aborts on a multivalued field', async () => {
    const metarecord = async () => ({
      version: 1,
      fields: [
        { name: 'tag', value: { type: 'string', value: 'a' } },
        { name: 'tag', value: { type: 'string', value: 'b' } },
      ],
    });
    const out = await expandShellPlaceholders('echo %tag', deps({ metarecord }));
    expect(out.ok).toBe(false);
    if (!out.ok) expect(out.error).toMatch(/multi/i);
  });

  test('aborts when a TreeRef path is multivalued', async () => {
    const treePaths = async () => ['/a', '/b'];
    const out = await expandShellPlaceholders('echo %mfr_path:path', deps({ treePaths }));
    expect(out.ok).toBe(false);
  });

  test('suggests :path for a bare TreeRef field', async () => {
    const out = await expandShellPlaceholders('echo %mfr_path', deps());
    expect(out.ok).toBe(false);
    if (!out.ok) expect(out.error).toMatch(/:path/);
  });
});
