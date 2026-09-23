import { mount } from 'svelte';
import App from './App.svelte';
import './app.css';
import type { ClipSource } from './lib/ipc';

const target = document.getElementById('app');
if (!target) {
  // Svelte 5 does not need this check, but failing here with a sentence beats a stack
  // trace from inside the runtime if `index.html` ever loses the mount point.
  throw new Error('localplay: #app is missing from index.html');
}

/**
 * TEST-ONLY SEAM — the one way to hand this window a `ClipSource` that is not the Tauri IPC.
 *
 * `App` already takes its source as a prop whose default is the real `tauriIpc`, so all that
 * is missing for a browser with no Tauri runtime is a way to reach that prop from outside the
 * bundle. It has to be a global: vite exports nothing from the built bundle, so a
 * module-scoped setter would be unreachable from the page. Both halves below are required —
 * the `?test-clip-source` query string and the `globalThis.__localplayClipSource` object — and
 * a shipped window is loaded from `tauri://localhost/` (Windows: `http://tauri.localhost/`)
 * with no query string, so even something that managed to set the global could not arm this.
 * Nothing in the application reads either half, and with them absent the call below is exactly
 * the `mount(App, { target })` that was here before the seam existed.
 *
 * `scripts/shots.mjs` (Playwright, headless Chromium) is the only code that ever arms it.
 */
const armed = new URLSearchParams(location.search).has('test-clip-source');
const injected = (globalThis as { __localplayClipSource?: ClipSource }).__localplayClipSource;
const testSource = armed ? injected : undefined;

export default testSource
  ? mount(App, { target, props: { source: testSource } })
  : mount(App, { target });
