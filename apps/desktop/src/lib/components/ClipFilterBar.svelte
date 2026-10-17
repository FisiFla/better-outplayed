<script lang="ts">
  /**
   * Search box plus game and tag filters for the clip library, in one toolbar row. Pure
   * view: the filter value is owned by App and the matching is `filterClips` in `../clips`.
   */
  import { isFiltering, NO_FILTER } from '../clips';
  import type { ClipFilter } from '../clips';
  import Icon from './Icon.svelte';

  interface Props {
    filter: ClipFilter;
    games: { value: string; count: number }[];
    tags: { value: string; count: number }[];
    onChange: (filter: ClipFilter) => void;
  }

  let { filter, games, tags, onChange }: Props = $props();
</script>

<div class="bar">
  <label class="search">
    <Icon name="search" size={16} />
    <input
      type="search"
      placeholder="Search title, game, tag…"
      aria-label="Search clips"
      value={filter.query}
      oninput={(e) => onChange({ ...filter, query: e.currentTarget.value })}
    />
  </label>
  <select
    aria-label="Filter by game"
    value={filter.game ?? ''}
    onchange={(e) => onChange({ ...filter, game: e.currentTarget.value || null })}
  >
    <option value="">All games</option>
    {#each games as game (game.value)}
      <option value={game.value}>{game.value} ({game.count})</option>
    {/each}
  </select>
  <select
    aria-label="Filter by tag"
    value={filter.tag ?? ''}
    onchange={(e) => onChange({ ...filter, tag: e.currentTarget.value || null })}
  >
    <option value="">All tags</option>
    {#each tags as tag (tag.value)}
      <option value={tag.value}>#{tag.value} ({tag.count})</option>
    {/each}
  </select>
  {#if isFiltering(filter)}
    <button type="button" class="ghost" onclick={() => onChange(NO_FILTER)}>
      <Icon name="x" size={14} /> Clear
    </button>
  {/if}
</div>

<style>
  .bar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--s-2);
    padding: var(--s-2) var(--s-5) var(--s-4);
  }

  .search {
    position: relative;
    flex: 1 1 240px;
    max-width: 420px;
    color: var(--text-muted);
  }

  .search :global(svg) {
    position: absolute;
    left: 10px;
    top: 50%;
    transform: translateY(-50%);
    pointer-events: none;
  }

  .search input {
    width: 100%;
    box-sizing: border-box;
    padding: 6px 10px 6px 32px;
  }

  select {
    min-width: 0;
    padding: 6px 10px;
  }

  button {
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }
</style>
