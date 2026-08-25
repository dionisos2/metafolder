import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vitest/config';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// Panel files import the shim helpers by the URL the Rust server serves them at
// (`import { el } from '/__ui.js'`). That served-URL → source-file map lives in
// tsconfig.json `paths` — TypeScript needs it to resolve panel code — so derive
// the Vite alias from it rather than restating it: a shim module added to one is
// added to both, and tests/served-modules.test.ts pins both against the Rust
// routes. (They had already drifted: /__paged-list.js and /__help.js were served
// and imported, but absent here.)
const { compilerOptions } = JSON.parse(
  readFileSync(new URL('./tsconfig.json', import.meta.url), 'utf8'),
) as { compilerOptions: { paths: Record<string, [string]> } };

const shimAlias = Object.fromEntries(
  Object.entries(compilerOptions.paths).map(([url, [file]]) => [
    url,
    fileURLToPath(new URL(file, import.meta.url)),
  ]),
);

/** An absolute path under crates/gui — two thirds of the GUI's JS is above the vite root. */
const gui = (path: string) => fileURLToPath(new URL(`../${path}`, import.meta.url));

export default defineConfig({
  plugins: [svelte()],
  resolve: { alias: shimAlias },
  clearScreen: false,
  // The shared key matcher lives outside the Vite root (panel-shim/ is
  // also served to panel iframes by the Rust server).
  server: { port: 5173, strictPort: true, fs: { allow: ['..'] } },
  build: { target: 'esnext' },
  test: {
    environment: 'jsdom',
    // Vitest is rooted at crates/gui, NOT at the vite root (frontend/). Coverage
    // hard-filters to the test project's root — no `include` or `coverage.root`
    // gets around it — and panel-shim/ and the panel types live above frontend/.
    // Rooted at frontend/ the report silently covers the Svelte shell alone (a
    // third of the GUI) while reading as if it covered everything. Vite itself
    // keeps frontend/ as its root for dev and build. (Resolving vitest's own
    // packages from up here is why the repo root is an npm workspace.)
    root: gui('.'),
    include: ['frontend/tests/**/*.test.ts'],
    coverage: {
      provider: 'v8',
      // `all` counts files no test imports — without it the 12 panel main.js
      // would not appear at all, and the number would flatter us by measuring
      // only what is already tested.
      all: true,
      include: [
        'frontend/src/**/*.{ts,svelte}',
        'panel-shim/**/*.js',
        'default-config/panel-types/**/*.js',
      ],
      // The entry point and the ipc mock seam have nothing to cover.
      exclude: ['frontend/src/main.ts', 'frontend/src/lib/ipc.ts'],
      reporter: ['text-summary'],
      reportsDirectory: 'frontend/coverage',
      // The measured floor, not an aspiration. A ratchet, so the number cannot
      // quietly fall; raise it as the panels get tested, never lower it to go
      // green.
      //
      // Statements jumped (35% → 58%) when panel-mount.test.ts started mounting
      // every panel, which runs each panel's whole mount body. Functions fell in
      // the same move (73% → 46%): mounting a panel *defines* its handlers —
      // dozens per panel — without calling any of them, so they land in the
      // report as uncovered. The two numbers moving in opposite directions is
      // the honest picture, not a regression.
      //
      // Branches behave the same way and for the same reason: v8 only reports
      // the branches of code it actually ran, so a test that drives a panel
      // nobody had driven before adds far more branches to the denominator than
      // it covers. repos.test.ts (the first behavioural test of the repos panel)
      // raised statements 61 → 63 and functions 48 → 50 while branches went
      // 85.1 → 84.0. The floor follows the measurement — it is never lowered to
      // turn a red run green, only moved when the number it tracks has moved.
      thresholds: { statements: 63, branches: 84, functions: 50, lines: 63 },
    },
  },
});
