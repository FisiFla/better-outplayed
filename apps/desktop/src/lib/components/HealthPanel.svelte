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

  interface Props {
    health: Health;
  }

  let { health }: Props = $props();
  const attention = $derived(needsAttention(health));
</script>

<section class="panel health">
  <h2>Health</h2>
  <details>
    <summary>
      {describeRecorderState(health.recorder_state, null)}{#if attention}
        <span class="flag"> (needs attention)</span>{/if}
    </summary>
    <dl>
      <div class="wide">
        <dt>Recorder</dt>
        <dd>{describeRecorderState(health.recorder_state, health.recorder_error)}</dd>
      </div>
      <div class="wide">
        <dt>Encoder</dt>
        <dd>{describeEncoder(health)}</dd>
      </div>
      <div class="wide">
        <dt>Disk (clips)</dt>
        <dd class:low={diskIsLow(health.disk)}>
          {describeDisk(health.disk)}{#if diskIsLow(health.disk)} (low){/if}
        </dd>
      </div>
      <div class="wide">
        <dt>Event sources</dt>
        {#if health.event_sources.length === 0}
          <dd class="muted">Start a recording to see them; they run only while it does.</dd>
        {:else}
          {#each health.event_sources as src (src.source)}
            <dd class:low={src.state === 'failed'}>
              {src.source}: {src.state}<span class="muted"> ({src.detail})</span>
            </dd>
          {/each}
        {/if}
      </div>
      <div class="wide">
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
      <div class="wide">
        <dt>Log file</dt>
        <dd class="path">{health.log_path}</dd>
      </div>
    </dl>
  </details>
</section>

<style>
  summary {
    cursor: pointer;
    color: var(--muted);
    font-size: 11px;
    padding: 2px 0;
  }

  summary:hover {
    color: var(--text);
  }

  .flag,
  .low {
    color: var(--warn);
  }

  dl {
    display: grid;
    gap: 8px;
    margin: 8px 0 0;
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

  .errors {
    margin: 0;
    padding-left: 14px;
    font-size: 11px;
    overflow-wrap: anywhere;
  }
</style>
