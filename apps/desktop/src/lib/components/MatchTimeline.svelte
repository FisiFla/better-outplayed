<script lang="ts">
  /**
   * The session split into matches: one bar per session with each match as a span, then a
   * card per match with its length, markers and (League) end-of-game scoreboard.
   * Extracting a match is the same lossless cut as extracting a range.
   */
  import { markerColor } from '../markers';
  import {
    canExtractMatch,
    kda,
    matchCaveat,
    matchLength,
    matchSpan,
    matchTitle,
    scoreboardByTeam,
  } from '../matches';
  import { formatPrecise } from '../time';
  import type { MatchDto } from '../types';

  interface Props {
    matches: MatchDto[];
    /** Session media length, `0` when unknown. */
    durationMs: number;
    extractable: boolean;
    busy: boolean;
    onExtract: (index: number) => void;
  }

  let { matches, durationMs, extractable, busy, onExtract }: Props = $props();
</script>

{#if matches.length > 0}
  <section class="matches">
    <h3>Matches ({matches.length})</h3>
    {#if durationMs > 0}
      <div class="axis" aria-hidden="true">
        {#each matches as m (m.index)}
          {@const span = matchSpan(m, durationMs)}
          {#if span !== null}
            <div class="span" style:left="{span.leftPct}%" style:width="{span.widthPct}%">{m.index + 1}</div>
          {/if}
        {/each}
      </div>
    {/if}

    {#each matches as m (m.index)}
      {@const caveat = matchCaveat(m)}
      <article class="match">
        <header>
          <strong>{matchTitle(m)}</strong>
          <span class="muted mono">
            {formatPrecise(m.start_ms)}–{formatPrecise(m.end_ms)} ({matchLength(m)})
          </span>
          <button
            type="button"
            disabled={!extractable || busy || !canExtractMatch(m)}
            onclick={() => onExtract(m.index)}
          >
            Extract full match
          </button>
        </header>
        {#if caveat !== null}<p class="muted caveat">{caveat}</p>{/if}
        <div class="dots">
          {#each m.events as e (e.id)}
            <span class="dot" style:background={markerColor(e.kind)} title="{e.kind} · {formatPrecise(e.offset_ms)}"></span>
          {/each}
        </div>
        {#if m.scoreboard.length > 0}
          <table class="score">
            <caption class="muted">End-of-game scoreboard</caption>
            <thead><tr><th>Player</th><th>Champion</th><th>K/D/A</th><th>CS</th></tr></thead>
            {#each scoreboardByTeam(m.scoreboard) as group (group.team)}
              <tbody>
                <tr class="team"><th colspan="4">{group.team === '' ? 'Unassigned' : group.team}</th></tr>
                {#each group.rows as r, i (i)}
                  <tr class:local={r.is_local}>
                    <td>{r.name ?? '—'}</td>
                    <td>{r.champion ?? '—'}</td>
                    <td class="mono">{kda(r)}</td>
                    <td class="mono">{r.cs}</td>
                  </tr>
                {/each}
              </tbody>
            {/each}
          </table>
        {/if}
      </article>
    {/each}
  </section>
{/if}

<style>
  .matches { display: flex; flex-direction: column; gap: 8px; }
  h3 { margin: 0; font-size: 13px; }
  .axis { position: relative; height: 18px; background: var(--surface, #222); border-radius: 4px; }
  .span {
    position: absolute; top: 0; bottom: 0; min-width: 4px; overflow: hidden;
    background: var(--accent, #4a8); color: #000; font-size: 11px; text-align: center;
    border-radius: 3px; opacity: 0.85;
  }
  .match { border: 1px solid var(--border, #444); border-radius: 6px; padding: 8px 10px; display: flex; flex-direction: column; gap: 6px; }
  .match header { display: flex; align-items: baseline; gap: 12px; flex-wrap: wrap; }
  .caveat { margin: 0; font-size: 11px; }
  .dots { display: flex; flex-wrap: wrap; gap: 3px; }
  .dot { width: 8px; height: 8px; border-radius: 50%; }
  .score { font-size: 12px; border-collapse: collapse; width: 100%; }
  .score th, .score td { text-align: left; padding: 1px 8px 1px 0; }
  .score .team th { color: var(--muted); font-size: 11px; padding-top: 4px; }
  .score .local td { font-weight: 600; }
</style>
