// The shim modules panels import by served URL (`import { el } from '/__ui.js'`)
// are declared in three places that must agree, or things break in ways nothing
// else catches:
//
//   - the Rust routes (server/mod.rs) — what the GUI actually serves at runtime;
//   - tsconfig `paths` — what TypeScript resolves when checking panel code;
//   - vite's `resolve.alias` — what vitest resolves when a test loads panel code.
//
// vite.config.ts derives its alias from the tsconfig, so those two cannot drift.
// This test pins the remaining pair — Rust vs tsconfig — and, more usefully,
// checks both against the URLs the panel sources *actually* import: a shim module
// that is imported but not served is a 404 at mount, and one that is served but
// never imported is dead weight in the binary.

import { readFileSync, readdirSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { describe, expect, test } from 'vitest';

// vitest runs with cwd = crates/gui/frontend (jsdom leaves `import.meta.url`
// unusable as a file: URL, hence the cwd-relative resolution).
const frontendDir = process.cwd();
const guiDir = resolve(frontendDir, '..');

const sorted = (urls: Iterable<string>) => [...new Set(urls)].sort();

/** The `/__*.js` keys of tsconfig `paths`, and the files they point at. */
function tsconfigPaths(): Record<string, string[]> {
  const text = readFileSync(join(frontendDir, 'tsconfig.json'), 'utf8');
  return JSON.parse(text).compilerOptions?.paths ?? {};
}

/** The `/__*.js` routes the Rust server exposes. */
function servedRoutes(): string[] {
  const text = readFileSync(join(guiDir, 'src/server/mod.rs'), 'utf8');
  return sorted([...text.matchAll(/\.route\("(\/__[\w-]+\.js)"/g)].map((m) => m[1]));
}

/** Every `/__*.js` specifier imported by the shim or by a panel type. */
function importedUrls(): string[] {
  const roots = [join(guiDir, 'panel-shim'), join(guiDir, 'default-config/panel-types')];
  const urls: string[] = [];
  for (const root of roots) {
    for (const file of readdirSync(root, { recursive: true, withFileTypes: true })) {
      if (!file.isFile() || !file.name.endsWith('.js')) continue;
      const text = readFileSync(join(file.parentPath, file.name), 'utf8');
      for (const m of text.matchAll(/from '(\/__[\w-]+\.js)'/g)) urls.push(m[1]);
    }
  }
  return sorted(urls);
}

describe('served shim modules', () => {
  test('tsconfig paths match the Rust routes', () => {
    expect(sorted(Object.keys(tsconfigPaths()))).toEqual(servedRoutes());
  });

  test('every URL the sources import is served', () => {
    expect(importedUrls().filter((u) => !servedRoutes().includes(u))).toEqual([]);
  });

  test('every served module is imported by something', () => {
    expect(servedRoutes().filter((u) => !importedUrls().includes(u))).toEqual([]);
  });

  test('each tsconfig path points at an existing shim file', () => {
    for (const [url, [target]] of Object.entries(tsconfigPaths())) {
      expect(() => readFileSync(resolve(frontendDir, target)), `${url} → ${target}`).not.toThrow();
    }
  });
});
