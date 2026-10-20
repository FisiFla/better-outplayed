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
  import { activeSource, errorMessage } from '../ipc';
  import type { ClipSource, DiagnosticsReport } from '../ipc';
  import type { Health } from '../types';
  import Icon from './Icon.svelte';

  interface Props {
    health: Health;
    /**
     * The command seam: the real Tauri IPC, or whatever a test or the screenshot harness
     * injects. See `activeSource`.
     */
    source?: ClipSource;
  }

  let { health, source = activeSource() }: Props = $props();
  const attention = $derived(needsAttention(health));
  const recorderBad = $derived(health.recorder_state === 'failed');
  const encoderBad = $derived(!health.ffmpeg_available);
  const diskLow = $derived(diskIsLow(health.disk));

  /** The report that was copied, once one has been. */
  let report = $state<DiagnosticsReport | null>(null);
  /** A refusal from the backend, in words the user can act on. */
  let error = $state<string | null>(null);
  /** The copy is in flight; the button is held while it is. */
  let busy = $state(false);

  function isDiagnosticsReport(value: unknown): value is DiagnosticsReport {
    return typeof value === 'object' && value !== null && 'version' in value;
  }

  /**
   * Put a diagnostics report on the clipboard.
   *
   * The report never leaves the machine by itself: it is on the clipboard, and the user
   * pastes it wherever they want to — a bug report, a forum, a friend. This application has
   * no telemetry, and the button says so rather than letting the word "diagnostics" imply a
   * upload.
   */
  async function copyDiagnostics(): Promise<void> {
    busy = true;
    error = null;
    try {
      const copied = await source.copyDiagnostics();
      report = isDiagnosticsReport(copied) ? copied : null;
      if (report === null) {
        error = 'The diagnostics command answered with nothing.';
      }
    } catch (err) {
      error = errorMessage(err);
    } finally {
      busy = false;
    }
  }
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
    <div class="row group diagnostics">
      <Icon name="copy" />
      <dt>Diagnostics</dt>
      <dd>
        <p class="muted footnote">
          One click copies a report — this build, this machine's configuration with the
          secrets redacted, and the end of the log — to the clipboard. Nothing is sent
          anywhere: the report stays on this machine until you paste it wherever you want to.
        </p>
        <button type="button" onclick={() => void copyDiagnostics()} disabled={busy}>
          Copy diagnostics
        </button>
        {#if error !== null}
          <p class="invalid" role="alert">{error}</p>
        {/if}
        {#if report !== null}
          <p class="copied" role="status">
            Copied to the clipboard: version {report.version}, {report.platform}, taken at
            {report.generated_at}.
          </p>
          <details>
            <summary>What was copied</summary>
            <p class="muted footnote">
              Version {report.version} · {report.platform} · {report.generated_at}
            </p>
            {#if report.problems.length === 0}
              <p class="muted footnote">No problems were found.</p>
            {:else}
              <p class="warn-text">Problems found:</p>
              <ul class="problems">
                {#each report.problems as problem (problem)}
                  <li>{problem}</li>
                {/each}
              </ul>
            {/if}
            <p class="muted footnote">Configuration (secrets redacted):</p>
            <pre class="mono block">{report.config}</pre>
            <p class="muted footnote">The end of the log:</p>
            <pre class="mono block">{report.log_tail}</pre>
          </details>
        {/if}
      </dd>
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

  .diagnostics dd {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: var(--s-2);
  }

  .diagnostics button {
    align-self: flex-start;
  }

  .copied {
    color: var(--ok);
    font: var(--t-small);
  }

  .invalid {
    color: var(--warn);
    font: var(--t-small);
  }

  .warn-text {
    color: var(--warn);
    font: var(--t-small);
  }

  .problems {
    margin: 0;
    padding-left: var(--s-4);
    font: var(--t-small);
    overflow-wrap: anywhere;
  }

  details {
    width: 100%;
    font: var(--t-small);
  }

  summary {
    cursor: pointer;
    color: var(--text-muted);
  }

  .block {
    margin: 0;
    padding: var(--s-2);
    max-height: 220px;
    overflow: auto;
    background: var(--surface-1);
    border: 1px solid var(--border);
    border-radius: var(--r-sm);
    white-space: pre-wrap;
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
