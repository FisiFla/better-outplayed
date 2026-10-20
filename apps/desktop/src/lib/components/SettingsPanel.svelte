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
  import type {
    AudioDevices,
    GsiGame,
    GsiTarget,
    RecordingStatus,
    SettingsDto,
    SettingsUpdate,
  } from '../types';
  import { QUALITY_OPTIONS, captureUpdate, deviceOptions, monitorIndex } from '../settings';
  import EncoderPanel from './EncoderPanel.svelte';

  interface Props {
    /** `null` until the first read answers; the panel is hidden meanwhile. */
    settings: SettingsDto | null;
    /** The engine's live counters; `null` until the first poll answers. */
    recording: RecordingStatus | null;
    /** A command is in flight; the apply buttons are held while it is. */
    busy?: boolean;
    onApply: (update: SettingsUpdate) => void;
    /**
     * Open the OS folder picker for the clips directory, through the backend.
     *
     * The backend runs the picker so that the directory it returns is one it has
     * approved for the asset-protocol read scope.
     */
    onChooseClipsDir: () => Promise<string>;
    /** The active microphones and outputs; empty lists leave only "Windows default". */
    audioDevices?: AudioDevices;
    /** Re-read the device list, so a device plugged in after launch shows up. */
    onRefreshAudioDevices?: () => void;
    /** CS2 / Dota 2 as Steam has them; empty until the first detection answers. */
    gsiTargets?: GsiTarget[];
    /** Write that game's Game State Integration cfg into its detected folder. */
    onInstallGsi?: (game: GsiGame) => void;
  }

  let {
    settings,
    recording = null,
    busy = false,
    onApply,
    onChooseClipsDir,
    audioDevices = { inputs: [], outputs: [] },
    onRefreshAudioDevices = () => {},
    gsiTargets = [],
    onInstallGsi = () => {},
  }: Props = $props();

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
  let saveSession = $state(false);
  let autoRecord = $state(false);
  let quality = $state('balanced');
  let monitorText = $state('0');
  let windowTitle = $state('');
  let micDevice = $state('');
  let outputDevice = $state('');

  $effect(() => {
    if (settings === null) return;
    clipsDir = settings.clips_dir;
    fpsText = String(settings.fps);
    outputSize = settings.output_size;
    micEnabled = settings.mic_enabled;
    saveSession = settings.save_session;
    autoRecord = settings.auto_record;
    quality = settings.quality;
    monitorText = String(settings.capture_monitor);
    windowTitle = settings.capture_window;
    micDevice = settings.mic_device;
    outputDevice = settings.output_device;
  });

  const micOptions = $derived(deviceOptions(audioDevices.inputs, micDevice));
  const outputOptions = $derived(deviceOptions(audioDevices.outputs, outputDevice));

  const fps = $derived(/^\d+$/.test(fpsText.trim()) ? Number(fpsText.trim()) : null);
  const fpsValid = $derived(fps !== null && fps >= 1 && fps <= 240);
  const monitor = $derived(monitorIndex(monitorText));
  const recordingNow = $derived(recording !== null && recording.running);
  const isDefault = $derived(
    (key: string) => settings?.defaulted.includes(key) ?? false,
  );

  function applyLibrary() {
    onApply({ clips_dir: clipsDir });
  }

  /**
   * The native folder picker, run by the backend.
   *
   * The backend runs it rather than the window so that the folder it returns is one it
   * has *approved*: only a directory the user picked in the OS dialog is added to the
   * asset-protocol read scope, so a path typed into the panel cannot widen what the
   * webview may read. Cancelling — or running somewhere with no Tauri runtime, like the
   * screenshot harness — resolves to no directory, and the draft is left alone rather
   * than cleared.
   */
  async function browseClipsDir() {
    // `Promise.resolve().then(...)` rather than a bare call: a picker that is missing or
    // throws synchronously is still a "no directory" to this panel, and a bare call would
    // let that throw escape as an uncaught page error instead.
    const selected = await Promise.resolve()
      .then(() => onChooseClipsDir())
      .catch(() => null);
    if (typeof selected === 'string') clipsDir = selected;
  }

  function applyCapture() {
    if (!fpsValid || fps === null) return;
    if (monitor === null || settings === null) return;
    onApply({
      fps,
      output_size: outputSize,
      mic_enabled: micEnabled,
      save_session: saveSession,
      ...captureUpdate(settings, {
        quality,
        monitor,
        window: windowTitle,
        micDevice,
        outputDevice,
      }),
    });
  }

  function applyAutomatic() {
    onApply({ auto_record: autoRecord });
  }

  /** The sections the sub-nav jumps between; the ids are local to this panel. */
  const SECTIONS = [
    { id: 'settings-library', label: 'Library' },
    { id: 'settings-capture', label: 'Capture' },
    { id: 'settings-encoder', label: 'Encoder' },
    { id: 'settings-automatic', label: 'Automatic recording' },
    { id: 'settings-games', label: 'Game integrations' },
  ];
  let activeSection = $state('settings-library');

  function jumpTo(id: string) {
    activeSection = id;
    document.getElementById(id)?.scrollIntoView({ block: 'start' });
  }

  /** Highlight the section nearest the top of the view as it scrolls (no-op without the API). */
  $effect(() => {
    if (settings === null || typeof IntersectionObserver === 'undefined') return;
    const seen = new Set<string>();
    const observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (entry.isIntersecting) seen.add(entry.target.id);
          else seen.delete(entry.target.id);
        }
        const first = SECTIONS.find((section) => seen.has(section.id));
        if (first) activeSection = first.id;
      },
      { rootMargin: '0px 0px -60% 0px' },
    );
    for (const section of SECTIONS) {
      const el = document.getElementById(section.id);
      if (el) observer.observe(el);
    }
    return () => observer.disconnect();
  });
</script>

<section class="panel settings">
  <header class="head">
    <h2>Settings</h2>
  </header>

  {#if settings !== null}
    <div class="layout">
    <nav class="subnav" aria-label="Settings sections">
      {#each SECTIONS as section (section.id)}
        <button
          type="button"
          class="ghost"
          aria-current={activeSection === section.id ? 'true' : undefined}
          onclick={() => jumpTo(section.id)}
        >
          {section.label}
        </button>
      {/each}
    </nav>

    <div class="sections">
    {#if !settings.config_exists}
      <p class="muted footnote">
        No config.toml yet — showing the example values. Saving writes the file.
      </p>
    {/if}

    <div class="group" id="settings-library">
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

    <div class="group" id="settings-capture">
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
      <label class="field">
        <span>Quality {#if isDefault('encode.bitrate_kbps')}<span class="muted">(default)</span>{/if}</span>
        <select
          value={quality}
          onchange={(event) => (quality = event.currentTarget.value)}
          disabled={busy}
        >
          {#if settings.quality === 'custom'}
            <option value="custom">Custom ({settings.bitrate_kbps} kbit/s)</option>
          {/if}
          {#each QUALITY_OPTIONS as option (option.value)}
            <option value={option.value}>{option.label}</option>
          {/each}
        </select>
      </label>
      <div class="row">
        <label class="field">
          <span>Monitor {#if isDefault('capture.monitor')}<span class="muted">(default)</span>{/if}</span>
          <input
            type="number"
            min="0"
            value={monitorText}
            oninput={(event) => (monitorText = event.currentTarget.value)}
            disabled={busy}
          />
        </label>
        <label class="field">
          <span>Window title {#if isDefault('capture.window')}<span class="muted">(default)</span>{/if}</span>
          <input
            type="text"
            value={windowTitle}
            oninput={(event) => (windowTitle = event.currentTarget.value)}
            placeholder="Whole monitor"
            disabled={busy}
            spellcheck={false}
          />
        </label>
      </div>
      {#if monitor === null}
        <p class="invalid">Monitor must be a number from 0 (the primary monitor).</p>
      {/if}
      <p class="muted footnote">
        Monitor 0 is the primary. A window title records that window instead; no match refuses to
        start. Windows only.
      </p>
      <label class="check">
        <input
          type="checkbox"
          checked={micEnabled}
          onchange={(event) => (micEnabled = event.currentTarget.checked)}
          disabled={busy}
        />
        <span>Record microphone {#if isDefault('mic.enabled')}<span class="muted">(default)</span>{/if}</span>
      </label>
      <div class="row">
        <label class="field">
          <span>Microphone device {#if isDefault('mic.device')}<span class="muted">(default)</span>{/if}</span>
          <select
            value={micDevice}
            onchange={(event) => (micDevice = event.currentTarget.value)}
            onfocus={onRefreshAudioDevices}
            disabled={busy || !micEnabled}
          >
            {#each micOptions as option (option.value)}
              <option value={option.value}>{option.label}</option>
            {/each}
          </select>
        </label>
        <label class="field">
          <span>Desktop audio device {#if isDefault('capture.output_device')}<span class="muted">(default)</span>{/if}</span>
          <select
            value={outputDevice}
            onchange={(event) => (outputDevice = event.currentTarget.value)}
            onfocus={onRefreshAudioDevices}
            disabled={busy}
          >
            {#each outputOptions as option (option.value)}
              <option value={option.value}>{option.label}</option>
            {/each}
          </select>
        </label>
      </div>
      <p class="muted footnote">
        Desktop audio is the output whose sound is recorded when the whole desktop is captured. A
        game's own audio is captured from the game itself, whichever device it plays on. A chosen
        device that is unplugged at start refuses the recording.
      </p>
      <label class="check">
        <input
          type="checkbox"
          checked={saveSession}
          onchange={(event) => (saveSession = event.currentTarget.checked)}
          disabled={busy}
        />
        <span>Also save the full session {#if isDefault('recorder.save_session')}<span class="muted">(default)</span>{/if}</span>
      </label>
      <p class="muted footnote">
        Writes the whole recording to a session file while the replay buffer keeps serving clips,
        from the same encode. Needs disk space; if the disk cannot keep up, the session file ends
        early and clips are unaffected.
      </p>
      {#if recordingNow}
        <p class="muted footnote">Takes effect on the next recording.</p>
      {/if}
      <button type="button" onclick={applyCapture} disabled={busy || !fpsValid || monitor === null}>
        Apply capture
      </button>
    </div>

    <EncoderPanel />

    <div class="group" id="settings-automatic">
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

    <div class="group" id="settings-games">
      <h3>Game integrations</h3>
      <p class="muted footnote">
        Counter-Strike 2 and Dota 2 send round and kill events to this app through a small
        file in the game's folder. The button writes that one file; nothing else in the game
        folder is touched. Restart the game afterwards.
      </p>
      {#each gsiTargets as target (target.game)}
        <div class="gsi-row">
          <span>{target.label}</span>
          {#if target.install_dir === null}
            <span class="muted">not found in Steam</span>
          {:else}
            <button type="button" onclick={() => onInstallGsi(target.game)} disabled={busy}>
              {target.installed ? 'Reinstall' : 'Install'}
            </button>
          {/if}
        </div>
      {/each}
    </div>
    </div>
    </div>
  {/if}
</section>

<style>
  .gsi-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }

  .settings {
    flex: none;
    gap: var(--s-4);
    padding: var(--s-5);
    border-top: 0;
  }

  .head h2 {
    font: var(--t-display);
    color: var(--text);
  }

  .layout {
    display: grid;
    grid-template-columns: 170px minmax(0, 1fr);
    gap: var(--s-5);
    align-items: start;
  }

  /* The sub-nav stays in view while the sections scroll past it. */
  .subnav {
    position: sticky;
    top: 0;
    display: flex;
    flex-direction: column;
    gap: var(--s-1);
  }

  .subnav button {
    text-align: left;
    border-color: transparent;
    color: var(--text-muted);
  }

  .subnav button[aria-current='true'] {
    background: var(--accent-soft);
    color: var(--text);
  }

  .sections {
    display: flex;
    flex-direction: column;
    gap: var(--s-4);
    min-width: 0;
  }

  .group {
    display: flex;
    flex-direction: column;
    gap: var(--s-2);
    padding: var(--s-4);
    background: var(--surface-2);
    border: 1px solid var(--border);
    border-radius: var(--r-lg);
    scroll-margin-top: var(--s-2);
  }

  h3 {
    margin: 0;
    font: var(--t-heading);
    color: var(--text);
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
    font: var(--t-small);
  }

  .field span {
    color: var(--text-muted);
  }

  .row {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(min(100%, 240px), 1fr));
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
    background: var(--surface-1);
    border: 1px solid var(--border-strong);
    border-radius: var(--r-sm);
    padding: 6px 10px;
    min-width: 0;
    max-width: 100%;
    text-overflow: ellipsis;
  }

  select {
    width: 100%;
  }

  input:disabled,
  select:disabled {
    opacity: 0.45;
  }

  .check {
    display: flex;
    align-items: center;
    gap: 8px;
    font: var(--t-small);
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
    font: var(--t-small);
  }

  .footnote {
    font: var(--t-small);
  }

  @media (max-width: 1000px) {
    .layout {
      grid-template-columns: minmax(0, 1fr);
    }

    .subnav {
      flex-direction: row;
      flex-wrap: wrap;
      position: static;
    }
  }

  .group button {
    align-self: flex-start;
  }
</style>
