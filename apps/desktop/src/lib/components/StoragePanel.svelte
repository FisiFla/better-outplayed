<script lang="ts">
  /**
   * The storage panel: what is on disk, what the cap is, and what the policy makes of it
   * (spec §8.1).
   *
   * It is READ-ONLY, and says so. The cleanup pass itself belongs to the recorder
   * (`localplay-cli`), which runs it at startup and on a timer while it is capturing; this
   * window reports the verdict that `plan_cleanup` reaches and never deletes anything on
   * its own initiative. A panel that appeared to manage storage without being able to
   * would be worse than one that explains who does.
   */
  import { formatBytes, percentOf } from '../time';
  import type { StorageStats } from '../types';

  interface Props {
    stats: StorageStats;
  }

  let { stats }: Props = $props();

  const usedPercent = $derived(percentOf(stats.total_bytes, stats.cap_bytes));
  const favouritePercent = $derived(percentOf(stats.favourite_bytes, stats.cap_bytes));
</script>

<section class="panel storage">
  <h2>Storage</h2>

  <div class="bar" role="img" aria-label="{usedPercent.toFixed(0)}% of the cap is used">
    <span class="favourites" style="width: {favouritePercent}%"></span>
    <span class="used" style="width: {Math.max(0, usedPercent - favouritePercent)}%"></span>
  </div>

  <dl>
    <div>
      <dt>Clips on disk</dt>
      <dd>{stats.clip_count} <span class="muted">({stats.favourite_count} favourite)</span></dd>
    </div>
    {#if stats.missing_count > 0}
      <!-- Counted nowhere above, so the panel says so here: the clips list filters rows
           whose file is gone, and a storage number that did not would contradict it. -->
      <div class="wide">
        <dt>Missing files</dt>
        <dd>
          {stats.missing_count}
          {stats.missing_count === 1 ? 'indexed clip' : 'indexed clips'} whose file is gone
          <span class="muted">
            — not counted or measured above; the rows stay in the index in case the file
            comes back
          </span>
        </dd>
      </div>
    {/if}
    <div>
      <dt>In use</dt>
      <dd>{formatBytes(stats.total_bytes)} <span class="muted">of {formatBytes(stats.cap_bytes)}</span></dd>
    </div>
    <div>
      <dt>Favourite bytes</dt>
      <dd>{formatBytes(stats.favourite_bytes)} <span class="muted">(exempt from every rule)</span></dd>
    </div>
    <div>
      <dt>Age limit</dt>
      <dd>{stats.max_age_days} days</dd>
    </div>
    <div class="wide">
      <dt>Clips directory</dt>
      <dd class="mono path">{stats.clips_dir}</dd>
    </div>
  </dl>

  {#if stats.cap_met}
    <p class="verdict ok">
      <strong>The cap can be met.</strong>
      {#if stats.planned_deletions > 0}
        The recorder's next cleanup pass would delete {stats.planned_deletions}
        {stats.planned_deletions === 1 ? 'clip' : 'clips'}, leaving
        {formatBytes(stats.bytes_after)}.
      {:else}
        Nothing is over the cap or past the age limit.
      {/if}
    </p>
  {:else}
    <p class="verdict bad">
      <strong>This cap cannot be met.</strong>
      The favourite clips alone exceed it by {formatBytes(stats.over_cap_by_bytes)}, and no
      cleanup pass may delete a favourite to satisfy a cap it cannot meet (spec §8.1) — so
      nothing else is deleted in the attempt either. Raise the cap, or un-protect the clips
      you no longer need.
    </p>
  {/if}

  {#each stats.warnings as warning}
    <p class="verdict bad">{warning}</p>
  {/each}

  <p class="muted footnote">
    Read-only: the recorder cleans up while it is capturing; this window deletes only the
    clip you ask it to.
  </p>
</section>

<style>
  .storage {
    flex: none;
  }

  .bar {
    display: flex;
    height: 8px;
    border-radius: 4px;
    background: var(--panel-2);
    border: 1px solid var(--line);
    overflow: hidden;
  }

  .used {
    background: var(--accent-dim);
  }

  .favourites {
    background: var(--star);
  }

  dl {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 6px 16px;
    margin: 0;
    font-size: 12px;
  }

  dl > div {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }

  dt {
    color: var(--muted);
    font-size: 11px;
  }

  dd {
    margin: 0;
  }

  .path {
    overflow-wrap: anywhere;
  }

  /*
   * The directory spans both columns. An absolute path is one unbreakable token longer than
   * a half-width column, and `overflow-wrap: anywhere` breaks it wherever it has to — which
   * in a 166px column means splitting it mid-word ("Videos\l / ocalplay\clips"). Full width
   * fits the path on one line instead.
   */
  dl > .wide {
    grid-column: 1 / -1;
  }

  .verdict {
    font-size: 12px;
    padding: 8px 10px;
    border-radius: 4px;
    border: 1px solid var(--line);
    background: var(--panel-2);
  }

  .verdict.ok {
    border-color: var(--accent-dim);
    background: rgba(94, 234, 212, 0.06);
  }

  .verdict.bad {
    border-color: var(--warn);
    background: rgba(251, 191, 36, 0.07);
  }

  .footnote {
    font-size: 11px;
  }
</style>
