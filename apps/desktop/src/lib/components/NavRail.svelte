<script lang="ts">
  /**
   * The left rail: five destinations, each an icon with a visible label. It is one tab stop
   * (roving tabindex) that the arrow keys move along; the active destination carries
   * `aria-current="page"`, and Health shows a dot when something on its page needs a look.
   */
  import { listIndex, RAIL_VIEWS } from '../nav';
  import type { NavKey, RailView } from '../nav';
  import type { IconName } from '../icons';
  import Icon from './Icon.svelte';

  interface Props {
    active: RailView;
    /** The Health item shows a dot (`healthBadge` in `nav.ts`). */
    healthAttention?: boolean;
    onNavigate: (view: RailView) => void;
  }

  let { active, healthAttention = false, onNavigate }: Props = $props();

  const ITEMS: Record<RailView, { label: string; icon: IconName }> = {
    library: { label: 'Library', icon: 'clapperboard' },
    sessions: { label: 'Sessions', icon: 'list-video' },
    storage: { label: 'Storage', icon: 'hard-drive' },
    health: { label: 'Health', icon: 'activity' },
    settings: { label: 'Settings', icon: 'settings' },
  };

  const buttons: Record<string, HTMLButtonElement | undefined> = {};

  function onKeydown(event: KeyboardEvent, index: number) {
    const keys = ['ArrowUp', 'ArrowDown', 'Home', 'End'];
    if (!keys.includes(event.key)) return;
    event.preventDefault();
    const next = listIndex(index, event.key as NavKey, RAIL_VIEWS.length);
    const target = RAIL_VIEWS[next];
    if (target !== undefined) buttons[target]?.focus();
  }
</script>

<nav class="rail" data-region="rail" aria-label="Main">
  <ul>
    {#each RAIL_VIEWS as view, index (view)}
      {@const item = ITEMS[view]}
      <li class:bottom={view === 'settings'}>
        <button
          type="button"
          class="ghost item"
          bind:this={buttons[view]}
          aria-current={active === view ? 'page' : undefined}
          tabindex={active === view ? 0 : -1}
          title={item.label}
          onclick={() => onNavigate(view)}
          onkeydown={(event) => onKeydown(event, index)}
        >
          <span class="glyph">
            <Icon name={item.icon} size={22} />
            {#if view === 'health' && healthAttention}
              <span class="dot" role="img" aria-label="needs attention"></span>
            {/if}
          </span>
          <span class="label">{item.label}</span>
        </button>
      </li>
    {/each}
  </ul>
</nav>

<style>
  .rail {
    display: flex;
    flex-direction: column;
    width: 64px;
    min-height: 0;
    padding: var(--s-2) 0;
    background: var(--bg);
    border-right: 1px solid var(--border);
  }

  ul {
    display: flex;
    flex: 1;
    flex-direction: column;
    gap: var(--s-1);
    margin: 0;
    padding: 0 var(--s-1);
    list-style: none;
  }

  li.bottom {
    margin-top: auto;
  }

  .item {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 3px;
    width: 100%;
    padding: var(--s-2) 0 6px;
    color: var(--text-muted);
    border-radius: var(--r-md);
  }

  .item:hover:not(:disabled) {
    color: var(--text);
  }

  .item[aria-current='page'] {
    color: var(--accent);
    background: var(--accent-soft);
  }

  .glyph {
    position: relative;
    display: inline-grid;
    place-items: center;
  }

  .label {
    font: var(--t-micro);
  }

  .dot {
    position: absolute;
    top: -2px;
    right: -4px;
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: var(--warn);
    border: 2px solid var(--bg);
  }
</style>
