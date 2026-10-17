<script lang="ts">
  /** A bundled Lucide glyph. Decorative: icon-only buttons carry their own `aria-label`. */
  import { ICONS } from '../icons';
  import type { IconName } from '../icons';

  interface Props {
    name: IconName;
    size?: number;
  }

  let { name, size = 18 }: Props = $props();
</script>

<svg
  aria-hidden="true"
  focusable="false"
  width={size}
  height={size}
  viewBox="0 0 24 24"
  fill="none"
  stroke="currentColor"
  stroke-width="1.75"
  stroke-linecap="round"
  stroke-linejoin="round"
>
  <!-- eslint-disable-next-line svelte/no-at-html-tags -- static markup from icons.ts, never user input -->
  {@html ICONS[name]}
</svg>
