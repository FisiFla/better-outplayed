<script lang="ts">
  /**
   * Search box plus game and tag filters for the clip list. Pure view: the filter value is
   * owned by App and the matching is `filterClips` in `../clips`.
   */
  import { isFiltering, NO_FILTER } from '../clips';
  import type { ClipFilter } from '../clips';

  interface Props {
    filter: ClipFilter;
    games: { value: string; count: number }[];
    tags: { value: string; count: number }[];
    onChange: (filter: ClipFilter) => void;
  }

  let { filter, games, tags, onChange }: Props = $props();
</script>

<div class="bar">
  <input
    type="search"
    placeholder="Search title, game, tag…"
    aria-label="Search clips"
    value={filter.query}
    oninput={(e) => onChange({ ...filter, query: e.currentTarget.value })}
  />
  <div class="selects">
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
      <button type="button" onclick={() => onChange(NO_FILTER)}>Clear</button>
    {/if}
  </div>
</div>

<style>
  .bar {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 0 8px 8px;
  }

  .selects {
    display: flex;
    gap: 6px;
  }

  select {
    flex: 1 1 0;
    min-width: 0;
  }
</style>
