import { defineConfig } from 'vitest/config';
import { svelte } from '@sveltejs/vite-plugin-svelte';

export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  // The shared key matcher lives outside the Vite root (panel-shim/ is
  // also served to panel iframes by the Rust server).
  server: { port: 5173, strictPort: true, fs: { allow: ['..'] } },
  build: { target: 'esnext' },
  test: { environment: 'jsdom' },
});
