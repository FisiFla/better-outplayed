<script lang="ts">
  /**
   * The session review window (spec §9): the clip list, the player with its scrubber, the
   * trim control, and the storage panel.
   *
   * Everything stateful lives here and every mutation goes through `ClipSource`, which is
   * a prop with the real Tauri IPC as its default. That is what lets the tests drive this
   * component's window against a fake source if they ever need to, without a Tauri runtime
   * or a display.
   */
  import { onMount } from 'svelte';
  import ClipDetail from './lib/components/ClipDetail.svelte';
  import ClipList from './lib/components/ClipList.svelte';
  import RecordingPanel from './lib/components/RecordingPanel.svelte';
  import SessionDetail from './lib/components/SessionDetail.svelte';
  import SessionList from './lib/components/SessionList.svelte';
  import StoragePanel from './lib/components/StoragePanel.svelte';
  import { describeDelete, describeTrim, resolveSelection, thumbnailAtMs, toClipViews } from './lib/clips';
  import type { ClipView } from './lib/clips';
  import { errorCode, errorMessage, tauriIpc } from './lib/ipc';
  import type { ClipSource } from './lib/ipc';
  import { describeRecordedClip } from './lib/recording';
  import {
    describeExtraction,
    describeSessionDelete,
    toSessionViews,
  } from './lib/sessions';
  import type { SessionView } from './lib/sessions';
  import type {
    AppStatus,
    RecordingStatus,
    SessionEvent,
    StorageStats,
    TrimRange,
  } from './lib/types';

  /**
   * How often the recorder's counters are read.
   *
   * They come from atomics on the recording thread (`RecorderHost::status`), so a poll
   * cannot disturb a capture that is trying to hold 60fps; half a second is under the
   * ~200ms tick the engine itself publishes on, which is the fastest this readout can
   * meaningfully change.
   */
  const RECORDING_POLL_MS = 500;

  interface Props {
    /** Injected so the window can be driven without the Tauri runtime. */
    source?: ClipSource;
  }

  let { source = tauriIpc }: Props = $props();

  let clips = $state<ClipView[]>([]);
  /** The sessions the index knows, newest first, as the sidebar lists them. */
  let sessions = $state<SessionView[]>([]);
  /** The markers of the selected session, read when it is selected. */
  let sessionEvents = $state<SessionEvent[]>([]);
  let stats = $state<StorageStats | null>(null);
  /** The engine's live counters; `null` until the first poll answers. */
  let recording = $state<RecordingStatus | null>(null);
  /** The clip hotkey and where the config came from; `null` until the first read answers. */
  let app = $state<AppStatus | null>(null);
  /** Clip id → `asset:` URL of its disk-cached thumbnail. */
  let thumbnails = $state<Record<number, string>>({});
  let selectedId = $state<number | null>(null);
  /** The selected session, if the main pane is showing one instead of a clip. */
  let selectedSessionId = $state<number | null>(null);
  /** A command is in flight; the controls that mutate are held while it is. */
  let busy = $state(false);
  /** A recording command is in flight (a start handshake, a stop, or a clip trigger). */
  let recordingBusy = $state(false);
  let error = $state<string | null>(null);
  let notice = $state<string | null>(null);
  let loaded = $state(false);

  const selected = $derived(clips.find((clip) => clip.id === selectedId) ?? null);
  const selectedSession = $derived(
    sessions.find((session) => session.id === selectedSessionId) ?? null,
  );

  async function refresh() {
    try {
      const [rows, storage, sessionRows] = await Promise.all([
        source.listClips(),
        source.storageStats(),
        source.listSessions(),
      ]);
      clips = toClipViews(rows, source.assetUrl);
      stats = storage;
      sessions = toSessionViews(sessionRows);
      selectedId = resolveSelection(clips, selectedId);
      // A selected session that is no longer in the list was deleted — here or by a retention
      // pass — so the main pane stops showing a session that does not exist.
      if (selectedSessionId !== null && !sessions.some((s) => s.id === selectedSessionId)) {
        selectedSessionId = null;
        sessionEvents = [];
      }
      error = null;
    } catch (err) {
      error = errorMessage(err);
    } finally {
      loaded = true;
    }
  }

  /**
   * Thumbnails, one clip at a time.
   *
   * Each cache miss is one ffmpeg process on the Rust side, so they are requested in
   * sequence rather than all at once — a library of 200 clips would otherwise start 200
   * decoders. Rust caches each result on disk, so this cost is paid once and a later start
   * answers from the cache.
   */
  async function loadThumbnails() {
    for (const clip of clips) {
      if (thumbnails[clip.id] !== undefined) continue;
      try {
        const thumb = await source.thumbnail(clip.id, thumbnailAtMs(clip.durationMs));
        thumbnails = { ...thumbnails, [clip.id]: source.assetUrl(thumb.path) };
      } catch (err) {
        // A clip with no thumbnail is a missing picture, not a broken library: say so once
        // and keep going, leaving the placeholder in that row.
        error = errorMessage(err);
        // ...unless ffmpeg is not there at all, in which case every remaining clip would
        // fail the same way and there is nothing to learn from trying them.
        if (errorCode(err) === 'ffmpeg_unavailable') return;
      }
    }
  }

  /**
   * Read the recorder's status. Never blocks the recording loop, and never throws into the
   * user's face: a failed poll is reported once through the banner and the readout keeps
   * the last values it had.
   *
   * One thing here is not just a readout: `clips` is the engine's own count of clips written
   * this session, and a *hotkey* press moves it without this window being involved at all.
   * When it grows, the clip that was just taken is in the index, so the list is refreshed —
   * otherwise a clip saved from the keyboard (or from the tray) would not appear until the
   * user happened to toggle something else.
   */
  async function refreshRecording() {
    try {
      const next = await source.recordingStatus();
      const grew = recording !== null && next.clips > recording.clips;
      recording = next;
      if (grew) {
        await refresh();
        void loadThumbnails();
      }
    } catch (err) {
      error = errorMessage(err);
    }
  }

  async function refreshApp() {
    try {
      app = await source.appStatus();
    } catch (err) {
      error = errorMessage(err);
    }
  }

  async function startRecording() {
    recordingBusy = true;
    try {
      recording = await source.startRecording();
      notice =
        'Recording. The buffer fills in the background; press Save clip for one now.';
    } catch (err) {
      error = errorMessage(err);
    } finally {
      recordingBusy = false;
    }
  }

  async function stopRecording() {
    recordingBusy = true;
    try {
      recording = await source.stopRecording();
      notice = 'Recording stopped; the encoder was flushed.';
    } catch (err) {
      error = errorMessage(err);
      // The stop may have failed *after* the recorder went away, so the readout is
      // refreshed either way rather than left claiming a recording that is not running.
      await refreshRecording();
    } finally {
      recordingBusy = false;
    }
  }

  /**
   * Take a clip now.
   *
   * This is the one command that takes real time: the engine waits for the post-roll to be
   * on disk (seconds) before it splices, so the button stays held while it runs. The clip
   * is written into the clips directory this window lists, which is why the list is
   * refreshed straight afterwards.
   */
  async function saveClip() {
    recordingBusy = true;
    try {
      const clip = await source.clipNow();
      notice = describeRecordedClip(clip);
      await refresh();
      void loadThumbnails();
    } catch (err) {
      error = errorMessage(err);
    } finally {
      recordingBusy = false;
    }
  }

  async function toggleFavourite(id: number, favourite: boolean) {
    busy = true;
    try {
      await source.setFavourite(id, favourite);
      notice = favourite
        ? 'Favourite: protected from both storage rules.'
        : 'No longer a favourite: the storage policy may delete this clip.';
      await refresh();
    } catch (err) {
      error = errorMessage(err);
    } finally {
      busy = false;
    }
  }

  async function deleteClip(id: number, name: string) {
    busy = true;
    try {
      const outcome = await source.deleteClip(id);
      notice = describeDelete(outcome, name);
      if (outcome.row_deleted) {
        // The cached thumbnail went with the clip; drop it so a later row cannot show a
        // picture of something that is no longer there.
        const { [id]: _dropped, ...rest } = thumbnails;
        thumbnails = rest;
      }
      await refresh();
    } catch (err) {
      error = errorMessage(err);
    } finally {
      busy = false;
    }
  }

  async function trimClip(id: number, range: TrimRange) {
    busy = true;
    try {
      const written = await source.trimClip(id, range.startMs, range.endMs);
      notice = describeTrim(range, written);
      await refresh();
      // Show the clip that was just made, and fetch its thumbnail.
      selectedId = written.id;
      void loadThumbnails();
    } catch (err) {
      error = errorMessage(err);
    } finally {
      busy = false;
    }
  }

  function clipName(id: number): string {
    return clips.find((clip) => clip.id === id)?.name ?? `clip #${id}`;
  }

  /**
   * Show a session's timeline in the main pane.
   *
   * The two selections are exclusive because there is one main pane, and a session's timeline
   * is not a clip's: selecting a session clears the clip, which is what makes the pane's
   * content unambiguous.
   *
   * The markers are read here rather than with the list, because they are per session and
   * nothing draws them until one is selected. An empty list is a real answer; a failure is
   * reported and the timeline simply has no markers, which is the same thing the user would
   * see for an untagged session.
   */
  async function selectSession(id: number) {
    selectedId = null;
    selectedSessionId = id;
    sessionEvents = [];
    try {
      sessionEvents = await source.sessionEvents(id);
    } catch (err) {
      error = errorMessage(err);
    }
  }

  async function extractClip(range: TrimRange) {
    const session = selectedSession;
    if (session === null) return;
    busy = true;
    try {
      const written = await source.extractClip(session.id, range.startMs, range.endMs);
      notice = describeExtraction(session.title, written);
      await refresh();
      void loadThumbnails();
      // Deliberately stays on the session: extracting several clips from one game is the
      // ordinary thing to do, and the notice names the clip that was just cut.
    } catch (err) {
      error = errorMessage(err);
    } finally {
      busy = false;
    }
  }

  async function toggleSessionFavourite(id: number, favourite: boolean) {
    busy = true;
    try {
      await source.setSessionFavourite(id, favourite);
      notice = favourite
        ? 'Favourite: this session is protected from both retention rules.'
        : 'No longer a favourite: the retention rules may delete this session.';
      await refresh();
    } catch (err) {
      error = errorMessage(err);
    } finally {
      busy = false;
    }
  }

  async function deleteSession(id: number) {
    const title = sessions.find((session) => session.id === id)?.title ?? `session #${id}`;
    busy = true;
    try {
      const outcome = await source.deleteSession(id);
      notice = describeSessionDelete(outcome, title);
      if (outcome.row_deleted && selectedSessionId === id) {
        selectedSessionId = null;
        sessionEvents = [];
      }
      await refresh();
    } catch (err) {
      // A session that is still recording is refused by the Rust side; the message says so,
      // and the row stays where it was.
      error = errorMessage(err);
    } finally {
      busy = false;
    }
  }

  onMount(() => {
    // Thumbnails are requested per clip, so they must wait for the list to arrive — which
    // is why this is one sequential step rather than three independent ones.
    void (async () => {
      await refresh();
      void loadThumbnails();
    })();
    // The recorder's counters are independent of the library, and are polled from here on.
    void refreshRecording();
    // Read once: a chord cannot change while the process runs (it is registered at startup),
    // and neither can the path the config came from.
    void refreshApp();
    const poll = setInterval(() => void refreshRecording(), RECORDING_POLL_MS);
    return () => clearInterval(poll);
  });
</script>

<div class="shell">
  <aside class="sidebar">
    <RecordingPanel
      status={recording}
      {app}
      busy={recordingBusy}
      onStart={startRecording}
      onStop={stopRecording}
      onClip={saveClip}
    />
    <div class="pane-head">
      <h2>Clips</h2>
      <span class="muted">{clips.length} indexed</span>
    </div>
    <ClipList
      {clips}
      {thumbnails}
      {selectedId}
      onSelect={(id) => {
        selectedId = id;
        // One main pane: selecting a clip takes it back from a session.
        selectedSessionId = null;
        sessionEvents = [];
      }}
      onFavourite={toggleFavourite}
      onDelete={(id) => deleteClip(id, clipName(id))}
    />
    <div class="pane-head">
      <h2>Sessions</h2>
      <span class="muted">{sessions.length} indexed</span>
    </div>
    <SessionList
      {sessions}
      selectedId={selectedSessionId}
      onSelect={selectSession}
      onFavourite={toggleSessionFavourite}
      onDelete={deleteSession}
    />
    {#if stats !== null}
      <StoragePanel {stats} />
    {/if}
  </aside>

  <main class="main">
    {#if error !== null}
      <div class="banner error" role="alert">
        <strong>Error.</strong>
        {error}
        <button type="button" onclick={() => (error = null)}>Dismiss</button>
      </div>
    {/if}

    {#if notice !== null}
      <div class="banner notice" role="status">
        {notice}
        <button type="button" onclick={() => (notice = null)}>Dismiss</button>
      </div>
    {/if}

    {#if selectedSession !== null}
      {@const session = selectedSession}
      {#key session.id}
        <SessionDetail
          {session}
          events={sessionEvents}
          {busy}
          onExtract={extractClip}
          onFavourite={(favourite) => toggleSessionFavourite(session.id, favourite)}
          onDelete={() => deleteSession(session.id)}
        />
      {/key}
    {:else if selected !== null}
      {@const clip = selected}
      {#key clip.id}
        <ClipDetail
          clip={clip}
          {busy}
          onTrim={(range) => trimClip(clip.id, range)}
          onFavourite={(favourite) => toggleFavourite(clip.id, favourite)}
        />
      {/key}
    {:else if loaded}
      <p class="empty">
        Select a clip on the left to play it, scrub it and trim it — or a session, to see its
        markers and cut a clip out of it.
      </p>
    {/if}
  </main>
</div>

<style>
  .banner {
    display: flex;
    align-items: baseline;
    gap: 8px;
  }

  .banner button {
    margin-left: auto;
    padding: 2px 8px;
    font-size: 11px;
  }

  .main .banner:first-child {
    margin-top: 14px;
  }

  .sidebar .pane-head {
    border-top: 1px solid var(--line);
  }
</style>
