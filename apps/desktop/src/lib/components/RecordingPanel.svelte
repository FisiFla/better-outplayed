<script lang="ts">
  /**
   * The recorder panel: start, stop, take a clip, and the live readout — plus the two facts
   * about this shell's *background* half that a user cannot otherwise discover: which chord
   * takes a clip (and whether it is really installed), and what closing the window does.
   *
   * This window OWNS nothing of the recording: the engine is `localplay-recorder` — the
   * same crate the CLI drives — and every number below comes from its status. What this
   * component decides is only what a person sees: three states (not recording, REC,
   * stopped with a reason), the counters while it runs, and two buttons.
   *
   * It deliberately has no controls for things that do not exist yet: no hotkey *binding*
   * (the chord is `[hotkeys] clip` in `config.toml`, and the panel names the file), no
   * encoder picker, no per-game event toggle. Those are settings (spec §10) or Phase 4, and
   * a disabled-looking control that cannot work is worse than no control.
   */
  import {
    describeAutoRecord,
    describeConfig,
    describeFrames,
    describeHotkey,
    formatDrift,
    formatRate,
    recordingLabel,
    recordingState,
  } from '../recording';
  import { formatBytes, formatDuration } from '../time';
  import type { AppStatus, RecordingStatus } from '../types';

  interface Props {
    /** `null` until the first status has arrived. */
    status: RecordingStatus | null;
    /** The background half — the hotkey, and where the config came from. `null` until read. */
    app: AppStatus | null;
    /** A command is in flight; the buttons are held while it is. */
    busy?: boolean;
    onStart: () => void;
    onStop: () => void;
    onClip: () => void;
  }

  let { status, app = null, busy = false, onStart, onStop, onClip }: Props = $props();

  const state = $derived(recordingState(status));
  const frames = $derived(describeFrames(status));
  const hotkey = $derived(describeHotkey(app?.hotkey ?? null));
  const config = $derived(describeConfig(app));
  const autoRecord = $derived(describeAutoRecord(status));
</script>

<section class="panel recorder">
  <div class="head">
    <h2>Recorder</h2>
    <span class="badge {state}">
      {#if state === 'recording'}<span class="dot" aria-hidden="true"></span>{/if}
      {recordingLabel(status)}
    </span>
  </div>

  {#if hotkey !== null}
    <p class="hotkey" class:bad={hotkey.problem}>{hotkey.text}</p>
  {/if}

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

  {#if state === 'recording' && status !== null}
    <p class="live">
      {formatDuration(status.span_ms)} of media · {formatRate(status)} ·
      {status.clips}
      {status.clips === 1 ? 'clip' : 'clips'} saved
    </p>
  {/if}

  {#if autoRecord !== null}
    <p class="muted auto">{autoRecord}</p>
  {/if}

  <details class="engine">
    <summary>Engine counters</summary>
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
    {:else}
      <p class="muted footnote">
        No status has arrived yet — the readout above says which read failed, if one did.
      </p>
    {/if}

    {#if state !== 'recording'}
      <p class="muted footnote">
        Nothing is being captured. Recording needs a GPU encoder (NVENC, Quick Sync or
        AMF); the CLI records through this same engine.
      </p>
    {/if}

    {#if app !== null}
      <p class="muted footnote shell-notes">
        {app.close_hint}{#if config !== null}<span class="config">{' '}{config}</span>{/if}
      </p>
    {/if}
  </details>
</section>

<style>
  .panel {
    border-top: none;
    border-bottom: 1px solid var(--line);
    flex: none;
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
    animation: rec-pulse 1.6s ease-out infinite;
  }

  @keyframes rec-pulse {
    0% {
      box-shadow: 0 0 0 0 rgba(248, 113, 113, 0.55);
    }
    70% {
      box-shadow: 0 0 0 6px rgba(248, 113, 113, 0);
    }
    100% {
      box-shadow: 0 0 0 0 rgba(248, 113, 113, 0);
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .dot {
      animation: none;
    }
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
    border-color: var(--warn);
    background: rgba(251, 191, 36, 0.07);
  }

  /*
   * The one line a recording shows: how much media the ring holds, at what rate, and how
   * many clips it produced. The full counters live in the disclosure below; this is the
   * line a glance is for.
   */
  .live {
    margin: 0;
    font-size: 12px;
  }

  .auto {
    margin: 0;
    font-size: 11px;
  }

  .engine {
    font-size: 12px;
  }

  .engine summary {
    cursor: pointer;
    color: var(--muted);
    font-size: 11px;
    padding: 2px 0;
  }

  .engine summary:hover {
    color: var(--text);
  }

  .engine .footnote {
    margin-top: 8px;
  }

  /*
   * The hotkey line: the sentence that tells a user which key to press, or — when nothing is
   * listening — the failure, which has to look like one. It is above the buttons on purpose:
   * it is the control that actually gets used, and it is the one that can be dead.
   */
  .hotkey {
    margin: 0;
    padding: 4px 8px;
    border: 1px solid var(--line);
    background: var(--panel-2);
    border-radius: 4px;
    font-size: 12px;
  }

  .hotkey.bad {
    border-color: var(--warn);
    background: rgba(251, 191, 36, 0.07);
  }

  .footnote {
    font-size: 11px;
  }

  .shell-notes .config {
    font-family: ui-monospace, SFMono-Regular, 'Cascadia Mono', Menlo, monospace;
    overflow-wrap: anywhere;
  }
</style>
