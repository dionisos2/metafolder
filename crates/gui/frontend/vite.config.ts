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

export default defineConfig({
  plugins: [svelte()],
  resolve: { alias: shimAlias },
  clearScreen: false,
  // The shared key matcher lives outside the Vite root (panel-shim/ is
  // also served to panel iframes by the Rust server).
  server: { port: 5173, strictPort: true, fs: { allow: ['..'] } },
  build: { target: 'esnext' },
  test: { environment: 'jsdom' },
});
