<script lang="ts">
  /**
   * The recorder panel: start, stop, take a clip, and the live readout.
   *
   * This window OWNS nothing of the recording: the engine is `localplay-recorder` — the
   * same crate the CLI drives — and every number below comes from its status. What this
   * component decides is only what a person sees: three states (not recording, REC,
   * stopped with a reason), the counters while it runs, and two buttons.
   *
   * It deliberately has no controls for things that do not exist yet: no hotkey binding, no
   * encoder picker, no per-game event toggle. Those are settings (spec §10) or Phase 4, and
   * a disabled-looking control that cannot work is worse than no control.
   */
  import {
    describeFrames,
    formatDrift,
    formatRate,
    recordingLabel,
    recordingState,
  } from '../recording';
  import { formatBytes, formatDuration } from '../time';
  import type { RecordingStatus } from '../types';

  interface Props {
    /** `null` until the first status has arrived. */
    status: RecordingStatus | null;
    /** A command is in flight; the buttons are held while it is. */
    busy?: boolean;
    onStart: () => void;
    onStop: () => void;
    onClip: () => void;
  }

  let { status, busy = false, onStart, onStop, onClip }: Props = $props();

  const state = $derived(recordingState(status));
  const frames = $derived(describeFrames(status));
</script>

<section class="panel recorder">
  <div class="head">
    <h2>Recorder</h2>
    <span class="badge {state}">
      {#if state === 'recording'}<span class="dot" aria-hidden="true"></span>{/if}
      {recordingLabel(status)}
    </span>
  </div>

  <div class="controls">
    {#if state === 'recording'}
      <button class="primary" type="button" onclick={onStop} disabled={busy}>Stop recording</button>
    {:else}
      <button class="primary" type="button" onclick={onStart} disabled={busy}>Start recording</button>
    {/if}
    <button type="button" onclick={onClip} disabled={busy || state !== 'recording'}>Save clip</button>
  </div>

  {#if status !== null && status.error !== null}
    <p class="verdict bad">
      <strong>The recorder stopped.</strong>
      {status.error}
    </p>
  {/if}

  {#if status !== null}
    <dl>
      <div>
        <dt>Buffered</dt>
        <dd>{formatDuration(status.span_ms)} <span class="muted">of media</span></dd>
      </div>
      <div>
        <dt>Segments</dt>
        <dd>{status.segments} <span class="muted">({formatBytes(status.bytes)})</span></dd>
      </div>
      <div>
        <dt>Frames</dt>
        <dd>{status.frames}</dd>
      </div>
      <div>
        <dt>Rate</dt>
        <dd>{formatRate(status)}</dd>
      </div>
      <div>
        <dt>Clips saved</dt>
        <dd>{status.clips}</dd>
      </div>
      <div>
        <dt>Media drift</dt>
        <dd>{formatDrift(status)}</dd>
      </div>
    </dl>

    {#if frames !== null}
      <p class="muted footnote">{frames}</p>
    {/if}

    {#if state === 'idle'}
      <p class="muted footnote">
        Nothing is being captured. Recording needs a GPU encoder (NVENC, Quick Sync or AMF)
        and writes to the same clips directory this window lists — the CLI records through
        this same engine.
      </p>
    {/if}
  {:else}
    <p class="muted footnote">
      Nothing is being captured, and the recorder has not been asked yet (or the last status
      read failed — the banner above says which). Recording needs a GPU encoder (NVENC,
      Quick Sync or AMF) and writes to the same clips directory this window lists; the CLI
      records through this same engine.
    </p>
  {/if}
</section>

<style>
  .panel {
    border-top: none;
    border-bottom: 1px solid var(--line);
  }

  .head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 8px;
  }

  .badge {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 1px 7px;
    border: 1px solid var(--line);
    border-radius: 999px;
    font-size: 11px;
    color: var(--muted);
    letter-spacing: 0.04em;
  }

  .badge.recording {
    color: var(--danger);
    border-color: var(--danger);
  }

  .badge.failed {
    color: var(--warn);
    border-color: var(--warn);
  }

  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--danger);
  }

  .controls {
    display: flex;
    gap: 8px;
  }

  .controls button {
    flex: 1;
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

  .verdict {
    font-size: 12px;
    padding: 8px 10px;
    border-radius: 4px;
    border: 1px solid var(--line);
    background: var(--panel-2);
  }

  .verdict.bad {
    border-left: 3px solid var(--warn);
  }

  .footnote {
    font-size: 11px;
  }
</style>
