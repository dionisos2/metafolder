// The builtins are tables, one per family (lib/builtins/): merged into one,
// a name two families declared would silently lose one of its handlers; and a
// handler for a name the Rust side does not register is a command nobody can
// find in the command input.

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, test, vi } from 'vitest';

vi.mock('../src/lib/ipc', () => ({
  invoke: vi.fn(async () => null),
  listen: vi.fn(async () => () => {}),
}));

const { builtinFamilies, builtins } = await import('../src/lib/builtins');

/** The names `register_builtins` registers (crates/gui/src/lib.rs). */
function registered(): Set<string> {
  const source = readFileSync(resolve(process.cwd(), '../src/lib.rs'), 'utf8');
  const start = source.indexOf('fn register_builtins(');
  const body = source.slice(start, source.indexOf('\n}\n', start));
  // One entry per tuple, its name first — on the tuple's line, or rustfmt's next.
  return new Set([...body.matchAll(/\(\s*"([a-z][a-z0-9:-]*)",/g)].map((m) => m[1]));
}

describe('builtin families', () => {
  test('no two families declare the same name', () => {
    const seen = new Map<string, string>();
    for (const [family, table] of Object.entries(builtinFamilies())) {
      for (const name of Object.keys(table)) {
        expect(seen.get(name), `${name} in ${family}`).toBeUndefined();
        seen.set(name, family);
      }
    }
    expect(Object.keys(builtins())).toHaveLength(seen.size);
  });

  test('every builtin handled is registered on the Rust side', () => {
    const rust = registered();
    expect(rust.size).toBeGreaterThan(50);
    const unregistered = Object.keys(builtins()).filter((name) => !rust.has(name));
    expect(unregistered).toEqual([]);
  });
});
