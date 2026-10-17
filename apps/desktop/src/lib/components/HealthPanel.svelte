<script lang="ts">
  /**
   * The health page: what the recorder, the encoder, the disk and the game-event sources are
   * doing, and the last problems the log holds. A readout only; the wording lives in
   * `lib/health.ts`.
   */
  import {
    describeDisk,
    describeEncoder,
    describeRecorderState,
    diskIsLow,
    needsAttention,
  } from '../health';
  import type { Health } from '../types';
  import Icon from './Icon.svelte';

  interface Props {
    health: Health;
  }

  let { health }: Props = $props();
  const attention = $derived(needsAttention(health));
  const recorderBad = $derived(health.recorder_state === 'failed');
  const encoderBad = $derived(!health.ffmpeg_available);
  const diskLow = $derived(diskIsLow(health.disk));
</script>

<section class="panel health">
  <header class="head">
    <h2>Health</h2>
    <p class="overall" class:flag={attention}>
      {describeRecorderState(health.recorder_state, null)}{#if attention}
        <span class="flag"> (needs attention)</span>{/if}
    </p>
  </header>

  <dl>
    <div class="row" class:bad={recorderBad}>
      <Icon name={recorderBad ? 'x' : 'circle-check'} />
      <dt>Recorder</dt>
      <dd>{describeRecorderState(health.recorder_state, health.recorder_error)}</dd>
    </div>
    <div class="row" class:bad={encoderBad}>
      <Icon name={encoderBad ? 'x' : 'circle-check'} />
      <dt>Encoder</dt>
      <dd>{describeEncoder(health)}</dd>
    </div>
    <div class="row" class:warn={diskLow}>
      <Icon name={diskLow ? 'info' : 'circle-check'} />
      <dt>Disk (clips)</dt>
      <dd class:low={diskLow}>
        {describeDisk(health.disk)}{#if diskLow} (low){/if}
      </dd>
    </div>
    <div class="row group">
      <Icon name="activity" />
      <dt>Event sources</dt>
      {#if health.event_sources.length === 0}
        <dd class="muted">Start a recording to see them; they run only while it does.</dd>
      {:else}
        <dd class="sources">
          {#each health.event_sources as src (src.source)}
            <span class="source" class:bad={src.state === 'failed'}>
              <Icon name={src.state === 'failed' ? 'x' : 'circle-check'} size={14} />
              <span class:low={src.state === 'failed'}>
                {src.source}: {src.state}<span class="muted"> ({src.detail})</span>
              </span>
            </span>
          {/each}
        </dd>
      {/if}
    </div>
    <div class="row group">
      <Icon name="info" />
      <dt>Recent warnings and errors</dt>
      {#if health.recent_errors.length === 0}
        <dd class="muted">None in the log.</dd>
      {:else}
        <dd>
          <ul class="errors">
            {#each health.recent_errors as line, i (i)}
              <li>{line}</li>
            {/each}
          </ul>
        </dd>
      {/if}
    </div>
    <div class="row">
      <Icon name="info" />
      <dt>Log file</dt>
      <dd class="path mono">{health.log_path}</dd>
    </div>
  </dl>
</section>

<style>
  .health {
    flex: none;
    gap: var(--s-4);
    padding: var(--s-5);
    border-top: 0;
  }

  .head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--s-3);
    flex-wrap: wrap;
  }

  .head h2 {
    font: var(--t-display);
    color: var(--text);
  }

  .overall {
    color: var(--text-muted);
    font: var(--t-body);
  }

  .flag,
  .low {
    color: var(--warn);
  }

  dl {
    display: grid;
    gap: var(--s-2);
    margin: 0;
  }

  /* A status row: glyph, label, value. Long values wrap under the label on narrow widths. */
  .row {
    display: grid;
    grid-template-columns: 18px 180px minmax(0, 1fr);
    align-items: start;
    column-gap: var(--s-3);
    padding: var(--s-3) var(--s-4);
    background: var(--surface-2);
    border: 1px solid var(--border);
    border-radius: var(--r-md);
    color: var(--ok);
  }

  .row.warn {
    color: var(--warn);
  }

  .row.bad {
    color: var(--danger);
  }

  .row.group {
    color: var(--text-muted);
  }

  .row :global(svg) {
    margin-top: 1px;
  }

  dt {
    color: var(--text-muted);
    font: var(--t-small);
    padding-top: 1px;
  }

  dd {
    margin: 0;
    color: var(--text);
    font: var(--t-body);
    overflow-wrap: anywhere;
  }

  dd.low {
    color: var(--warn);
  }

  .sources {
    display: flex;
    flex-direction: column;
    gap: var(--s-1);
  }

  .source {
    display: flex;
    align-items: center;
    gap: var(--s-2);
    color: var(--ok);
  }

  .source.bad {
    color: var(--danger);
  }

  .source > span {
    color: var(--text);
  }

  .source > span.low {
    color: var(--warn);
  }

  .path {
    overflow-wrap: anywhere;
  }

  .errors {
    margin: 0;
    padding-left: var(--s-4);
    font: var(--t-small);
    overflow-wrap: anywhere;
  }

  @media (max-width: 1000px) {
    .row {
      grid-template-columns: 18px minmax(0, 1fr);
    }

    .row > dd {
      grid-column: 2;
    }
  }
</style>
