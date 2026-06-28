// Placeholder substitution for `!` shell commands (lib/placeholders.ts):
// `%u`/`%v`/`%p`/`%r`/`%<field>`/`%<field>:path` expand to data from the
// selection and the active workspace. Parsing and substitution are pure; the
// async orchestrator is driven with stub deps. spec-gui "Command input".

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
      { ph: { name: 'u', mod: null } },
      { lit: ' ' },
      { ph: { name: 'v', mod: null } },
    ]);
  });

  test('field value and field path', () => {
    expect(parsePlaceholders('open %mfr_path:path then %tag')).toEqual([
      { lit: 'open ' },
      { ph: { name: 'mfr_path', mod: 'path' } },
      { lit: ' then ' },
      { ph: { name: 'tag', mod: null } },
    ]);
  });

  test('repo name modifier', () => {
    expect(parsePlaceholders('cd %p in %r:name')).toEqual([
      { lit: 'cd ' },
      { ph: { name: 'p', mod: null } },
      { lit: ' in ' },
      { ph: { name: 'r', mod: 'name' } },
    ]);
  });

  test('%% is a literal percent', () => {
    expect(parsePlaceholders('100%% done %u')).toEqual([
      { lit: '100' },
      { lit: '%' },
      { lit: ' done ' },
      { ph: { name: 'u', mod: null } },
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
    selectedPaths: async () => ['/music/a.txt'],
    activeRepo: async () => 'repo-1',
    repoName: async () => 'music',
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

  // ── %p (selected path) ───────────────────────────────────────────────

  test('expands %p from selected_paths', async () => {
    const out = await expandShellPlaceholders('cat %p', deps());
    expect(out).toEqual({ ok: true, value: "cat '/music/a.txt'" });
  });

  test('%p works without a selected metarecord (untracked file)', async () => {
    const out = await expandShellPlaceholders(
      'cat %p',
      deps({ selected: async () => null, selectedPaths: async () => ['/a b.txt'] }),
    );
    expect(out).toEqual({ ok: true, value: "cat '/a b.txt'" });
  });

  test('%p aborts when there is no selected path', async () => {
    const out = await expandShellPlaceholders('cat %p', deps({ selectedPaths: async () => [] }));
    expect(out.ok).toBe(false);
  });

  test('%p aborts when several paths are selected', async () => {
    const out = await expandShellPlaceholders(
      'cat %p',
      deps({ selectedPaths: async () => ['/a', '/b'] }),
    );
    expect(out.ok).toBe(false);
  });

  // ── %r (active repo) ─────────────────────────────────────────────────

  test('expands %r to the active repo uuid', async () => {
    const out = await expandShellPlaceholders('echo %r', deps());
    expect(out).toEqual({ ok: true, value: "echo 'repo-1'" });
  });

  test('expands %r:name to the active repo name', async () => {
    const repoName = vi.fn(async () => 'music');
    const out = await expandShellPlaceholders('echo %r:name', deps({ repoName }));
    expect(out).toEqual({ ok: true, value: "echo 'music'" });
    expect(repoName).toHaveBeenCalledWith('repo-1');
  });

  test('%r:uuid is the explicit uuid form', async () => {
    const out = await expandShellPlaceholders('echo %r:uuid', deps());
    expect(out).toEqual({ ok: true, value: "echo 'repo-1'" });
  });

  test('%r works without a selected metarecord', async () => {
    const out = await expandShellPlaceholders('echo %r', deps({ selected: async () => null }));
    expect(out).toEqual({ ok: true, value: "echo 'repo-1'" });
  });

  test('%r aborts when there is no active repo', async () => {
    const out = await expandShellPlaceholders('echo %r', deps({ activeRepo: async () => null }));
    expect(out.ok).toBe(false);
  });

  test('an unknown modifier aborts', async () => {
    const out = await expandShellPlaceholders('echo %r:bogus', deps());
    expect(out.ok).toBe(false);
  });
});
