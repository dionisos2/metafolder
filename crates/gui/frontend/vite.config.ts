import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vitest/config';
import { svelte } from '@sveltejs/vite-plugin-svelte';

export default defineConfig({
  plugins: [svelte()],
  resolve: {
    alias: {
      // Panel files import the UI helpers by their served URL; map it to
      // the source module so vitest can load panel code unchanged.
      '/__ui.js': fileURLToPath(new URL('../panel-shim/ui.js', import.meta.url)),
      '/__menu.js': fileURLToPath(new URL('../panel-shim/menu.js', import.meta.url)),
      '/__orphan.js': fileURLToPath(new URL('../panel-shim/orphan.js', import.meta.url)),
      '/__value-widget.js': fileURLToPath(
        new URL('../panel-shim/value-widget.js', import.meta.url),
      ),
      '/__schema-template.js': fileURLToPath(
        new URL('../panel-shim/schema-template.js', import.meta.url),
      ),
      '/__finder.js': fileURLToPath(new URL('../panel-shim/finder.js', import.meta.url)),
    },
  },
  clearScreen: false,
  // The shared key matcher lives outside the Vite root (panel-shim/ is
  // also served to panel iframes by the Rust server).
  server: { port: 5173, strictPort: true, fs: { allow: ['..'] } },
  build: { target: 'esnext' },
  test: { environment: 'jsdom' },
});
