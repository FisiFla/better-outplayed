/// <reference types="vitest/config" />
import { defineConfig } from 'vitest/config';
import { svelte } from '@sveltejs/vite-plugin-svelte';

/**
 * Tauri serves the built frontend from `dist/` (`build.frontendDist` in
 * `src-tauri/tauri.conf.json`), so the output directory must not be changed without
 * changing that file too.
 */
export default defineConfig({
  plugins: [svelte()],

  // Tauri prints its own build output; clearing the screen would throw it away.
  clearScreen: false,

  server: {
    // `strictPort` because the Tauri dev URL in `tauri.conf.json` names this port
    // exactly — silently moving to 5174 would leave the webview pointed at nothing.
    port: 5173,
    strictPort: true,
    watch: {
      // The Rust half is rebuilt by cargo, not by vite; watching it would restart the
      // dev server on every `cargo build` for no benefit.
      ignored: ['**/src-tauri/**'],
    },
  },

  build: {
    // No bundler can test this without a webview, so the target is chosen for the two
    // engines Tauri actually uses: WebView2 (Chromium) on Windows and WKWebView on the
    // macOS development host.
    target: 'es2022',
    sourcemap: true,
  },

  test: {
    // The suite covers the pure logic — time formatting, trim-range maths, the
    // clip-list mapping — and drives the IPC module with the Tauri `invoke` call
    // mocked. None of it needs a DOM, so no jsdom/happy-dom environment is pulled in.
    environment: 'node',
    include: ['src/**/*.test.ts'],
  },
});
