import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/**
 * `vitePreprocess` is what makes `<script lang="ts">` work inside `.svelte` files;
 * without it the Svelte compiler sees TypeScript as JavaScript and fails on the first
 * type annotation. It also handles the `lang="scss"`-style style preprocessors — the
 * app uses plain CSS, so only the script half matters here.
 */
export default {
  preprocess: vitePreprocess(),
};
