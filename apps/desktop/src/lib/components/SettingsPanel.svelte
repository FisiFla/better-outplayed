<script lang="ts">
  /**
   * The settings panel: what the recorder runs with, shown and edited in the window.
   *
   * Everything here persists to `config.toml` the moment it is applied — the file the
   * CLI reads too, so one edit means one thing to both front-ends. What applies *now*
   * versus on the next recording is the honest split the backend reports: the clips
   * directory moves future writes immediately (existing rows stay where their files
   * are), while fps, output size, mic and auto-record are baked in per recorder start.
   * The panel says which is which instead of pretending everything is live.
   */
  import type { RecordingStatus, SettingsDto, SettingsUpdate } from '../types';
  import { open } from '@tauri-apps/plugin-dialog';

  interface Props {
    /** `null` until the first read answers; the panel is hidden meanwhile. */
    settings: SettingsDto | null;
    /** The engine's live counters; `null` until the first poll answers. */
    recording: RecordingStatus | null;
    /** A command is in flight; the apply buttons are held while it is. */
    busy?: boolean;
    onApply: (update: SettingsUpdate) => void;
  }

  let { settings, recording = null, busy = false, onApply }: Props = $props();

  const RESOLUTIONS = [
    { label: 'Native (capture size)', value: '' },
    { label: '1920 × 1080', value: '1920x1080' },
    { label: '1280 × 720', value: '1280x720' },
    { label: '960 × 540', value: '960x540' },
  ];

  /** The drafts behind the three Apply buttons, reseeded whenever a fresh read lands. */
  let clipsDir = $state('');
  let fpsText = $state('60');
  let outputSize = $state('');
  let micEnabled = $state(false);
  let autoRecord = $state(false);

  $effect(() => {
    if (settings === null) return;
    clipsDir = settings.clips_dir;
    fpsText = String(settings.fps);
    outputSize = settings.output_size;
    micEnabled = settings.mic_enabled;
    autoRecord = settings.auto_record;
  });

  const fps = $derived(/^\d+$/.test(fpsText.trim()) ? Number(fpsText.trim()) : null);
  const fpsValid = $derived(fps !== null && fps >= 1 && fps <= 240);
  const recordingNow = $derived(recording !== null && recording.running);
  const isDefault = $derived(
    (key: string) => settings?.defaulted.includes(key) ?? false,
  );

  function applyLibrary() {
    onApply({ clips_dir: clipsDir });
  }

  /**
   * The native folder picker. Cancelling — or running somewhere with no Tauri
   * runtime, like the screenshot harness — resolves to no directory, and the draft
   * is left alone rather than cleared.
   */
  async function browseClipsDir() {
    const selected = await open({
      directory: true,
      multiple: false,
      title: 'Choose the clips directory',
    }).catch(() => null);
    if (typeof selected === 'string') clipsDir = selected;
  }

  function applyCapture() {
    if (!fpsValid || fps === null) return;
    onApply({ fps, output_size: outputSize, mic_enabled: micEnabled });
  }

  function applyAutomatic() {
    onApply({ auto_record: autoRecord });
  }
</script>

<section class="panel settings">
  <h2>Settings</h2>

  {#if settings !== null}
    <details class="body">
      <summary>Library, capture, automatic recording</summary>

    {#if !settings.config_exists}
      <p class="muted footnote">
        No config.toml yet — showing the example values. Saving writes the file.
      </p>
    {/if}

    <div class="group">
      <h3>Library</h3>
      <label class="field">
        <span>Clips directory {#if isDefault('storage.clips_dir')}<span class="muted">(default)</span>{/if}</span>
        <span class="dir-row">
          <input
            type="text"
            value={clipsDir}
            oninput={(event) => (clipsDir = event.currentTarget.value)}
            disabled={busy}
            spellcheck={false}
          />
          <button type="button" onclick={browseClipsDir} disabled={busy}>Browse</button>
        </span>
      </label>
      <p class="muted footnote">Empty resets to the default. Applies immediately to new clips.</p>
      <button type="button" onclick={applyLibrary} disabled={busy}>Apply library</button>
    </div>

    <div class="group">
      <h3>Capture</h3>
      <div class="row">
        <label class="field">
          <span>Framerate {#if isDefault('encode.fps')}<span class="muted">(default)</span>{/if}</span>
          <input
            type="number"
            min="1"
            max="240"
            value={fpsText}
            oninput={(event) => (fpsText = event.currentTarget.value)}
            disabled={busy}
          />
        </label>
        <label class="field">
          <span>Resolution {#if isDefault('encode.output_size')}<span class="muted">(default)</span>{/if}</span>
          <select
            value={outputSize}
            onchange={(event) => (outputSize = event.currentTarget.value)}
            disabled={busy}
          >
            {#each RESOLUTIONS as option (option.value)}
              <option value={option.value}>{option.label}</option>
            {/each}
          </select>
        </label>
      </div>
      {#if !fpsValid}
        <p class="invalid">Framerate must be 1–240.</p>
      {/if}
      <label class="check">
        <input
          type="checkbox"
          checked={micEnabled}
          onchange={(event) => (micEnabled = event.currentTarget.checked)}
          disabled={busy}
        />
        <span>Microphone track {#if isDefault('mic.enabled')}<span class="muted">(default)</span>{/if}</span>
      </label>
      {#if recordingNow}
        <p class="muted footnote">Takes effect on the next recording.</p>
      {/if}
      <button type="button" onclick={applyCapture} disabled={busy || !fpsValid}>
        Apply capture
      </button>
    </div>

    <div class="group">
      <h3>Automatic recording</h3>
      <label class="check">
        <input
          type="checkbox"
          checked={autoRecord}
          onchange={(event) => (autoRecord = event.currentTarget.checked)}
          disabled={busy}
        />
        <span>Record watched games automatically {#if isDefault('games.auto_record')}<span class="muted">(default)</span>{/if}</span>
      </label>
      <p class="muted footnote">Watched: {settings.watch_titles.join(', ')}</p>
      {#if recordingNow}
        <p class="muted footnote">Takes effect on the next recording.</p>
      {/if}
      <button type="button" onclick={applyAutomatic} disabled={busy}>Apply automatic</button>
    </div>
    </details>
  {/if}
</section>

<style>
  .body summary {
    cursor: pointer;
    color: var(--muted);
    font-size: 11px;
    padding: 2px 0;
  }

  .body summary:hover {
    color: var(--text);
  }

  .group {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding-top: 4px;
  }

  .group + .group {
    border-top: 1px solid var(--line);
    padding-top: 12px;
  }

  h3 {
    margin: 0;
    font-size: 12px;
    font-weight: 600;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 12px;
  }

  .field span {
    color: var(--muted);
    font-size: 11px;
  }

  .row {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
  }

  .dir-row {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 8px;
  }

  .dir-row input {
    min-width: 0;
  }

  input[type='text'],
  input[type='number'],
  select {
    font: inherit;
    color: inherit;
    background: var(--panel-2);
    border: 1px solid var(--line);
    border-radius: 6px;
    padding: 5px 8px;
    min-width: 0;
  }

  input:disabled,
  select:disabled {
    opacity: 0.45;
  }

  .check {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 12px;
    cursor: pointer;
  }

  .check input {
    width: 15px;
    height: 15px;
    margin: 0;
    accent-color: var(--accent);
    cursor: pointer;
  }

  .invalid {
    color: var(--warn);
    font-size: 11px;
  }

  .footnote {
    font-size: 11px;
  }

  .group button {
    align-self: flex-start;
  }
</style>
