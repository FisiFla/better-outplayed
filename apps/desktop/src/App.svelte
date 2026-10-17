<script lang="ts">
  /**
   * The session review window (spec §9): a left rail of destinations (library, sessions,
   * storage, health, settings), the recorder bar that stays above all of them, and the active
   * view.
   *
   * Everything stateful lives here and every mutation goes through `ClipSource`, which is
   * a prop with the real Tauri IPC as its default. That is what lets the tests drive this
   * component's window against a fake source if they ever need to, without a Tauri runtime
   * or a display.
   */
  import { onMount } from 'svelte';
  import ClipDetail from './lib/components/ClipDetail.svelte';
  import ClipFilterBar from './lib/components/ClipFilterBar.svelte';
  import ClipList from './lib/components/ClipList.svelte';
  import HealthPanel from './lib/components/HealthPanel.svelte';
  import Icon from './lib/components/Icon.svelte';
  import NavRail from './lib/components/NavRail.svelte';
  import RecorderBar from './lib/components/RecorderBar.svelte';
  import SessionDetail from './lib/components/SessionDetail.svelte';
  import SessionList from './lib/components/SessionList.svelte';
  import SettingsPanel from './lib/components/SettingsPanel.svelte';
  import StoragePanel from './lib/components/StoragePanel.svelte';
  import Toasts from './lib/components/Toasts.svelte';
  import {
    describeDelete,
    describeTrim,
    filterClips,
    gameOptions,
    isFiltering,
    NO_FILTER,
    tagOptions,
    thumbnailAtMs,
    toClipViews,
  } from './lib/clips';
  import type { ClipFilter, ClipView } from './lib/clips';
  import { errorCode, errorMessage, tauriIpc } from './lib/ipc';
  import type { ClipSource } from './lib/ipc';
  import { describeGsiInstall } from './lib/health';
  import {
    back,
    healthBadge,
    INITIAL_VIEW,
    navigate,
    openClip,
    openSession,
    railOf,
    reconcile,
  } from './lib/nav';
  import type { RailView, View } from './lib/nav';
  import { describeRecordedClip } from './lib/recording';
  import { describeSettingsUpdate } from './lib/settings';
  import {
    describeExtraction,
    describeSessionDelete,
    toSessionViews,
  } from './lib/sessions';
  import type { SessionView } from './lib/sessions';
  import type {
    AppStatus,
    GsiGame,
    GsiTarget,
    Health,
    RecordingStatus,
    MatchDto,
    SessionEvent,
    SettingsDto,
    SettingsUpdate,
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

  /** The health page reads the log's tail and the disk, so it is polled far less often. */
  const HEALTH_POLL_MS = 5000;

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
  /** The selected session split into matches. */
  let sessionMatches = $state<MatchDto[]>([]);
  let stats = $state<StorageStats | null>(null);
  /** The engine's live counters; `null` until the first poll answers. */
  let recording = $state<RecordingStatus | null>(null);
  /** The effective settings; `null` until the first read answers. */
  let settings = $state<SettingsDto | null>(null);
  /** The clip hotkey and where the config came from; `null` until the first read answers. */
  let app = $state<AppStatus | null>(null);
  /** The health page's data; `null` until the first read answers. */
  let health = $state<Health | null>(null);
  /** CS2 / Dota 2 as Steam has them, for the integration installer. */
  let gsiTargets = $state<GsiTarget[]>([]);
  /** Clip id → `asset:` URL of its disk-cached thumbnail. */
  let thumbnails = $state<Record<number, string>>({});
  /** Where the window is: a rail destination, or a clip / session opened from one. */
  let view = $state<View>(INITIAL_VIEW);
  /** A command is in flight; the controls that mutate are held while it is. */
  let busy = $state(false);
  /** A recording command is in flight (a start handshake, a stop, or a clip trigger). */
  let recordingBusy = $state(false);
  let error = $state<string | null>(null);
  let notice = $state<string | null>(null);
  let loaded = $state(false);

  /** Search text, game and tag filters over the library list. */
  let filter = $state<ClipFilter>(NO_FILTER);
  const visibleClips = $derived(filterClips(clips, filter));

  const selectedId = $derived(view.kind === 'clip' ? view.id : null);
  const selectedSessionId = $derived(view.kind === 'session' ? view.id : null);
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
      // An open clip or session that is no longer listed was deleted — here or by a retention
      // pass — so the window returns to its list instead of showing something that is gone.
      view = reconcile(
        view,
        clips.map((clip) => clip.id),
        sessions.map((session) => session.id),
      );
      if (view.kind !== 'session') {
        sessionEvents = [];
        sessionMatches = [];
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

  async function refreshHealth() {
    try {
      health = await source.health();
    } catch (err) {
      // A failing health read must not raise the main banner every five seconds.
      void source.logFromFrontend('warn', 'health read failed', errorMessage(err));
    }
  }

  async function refreshGsiTargets() {
    try {
      gsiTargets = await source.gsiTargets();
    } catch (err) {
      error = errorMessage(err);
    }
  }

  async function installGsi(game: GsiGame) {
    busy = true;
    try {
      const outcome = await source.installGsiCfg(game);
      const label = gsiTargets.find((t) => t.game === game)?.label ?? game;
      notice = describeGsiInstall(outcome, label);
      await refreshGsiTargets();
    } catch (err) {
      error = errorMessage(err);
    } finally {
      busy = false;
    }
  }

  async function refreshSettings() {
    try {
      settings = await source.getSettings();
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

  async function runClipAction(action: () => Promise<unknown>, done: string) {
    busy = true;
    try {
      await action();
      notice = done;
      await refresh();
    } catch (err) {
      error = errorMessage(err);
    } finally {
      busy = false;
    }
  }

  const renameClip = (id: number, title: string) =>
    runClipAction(
      () => source.setClipTitle(id, title),
      title.trim() === '' ? 'Title cleared: showing the file name.' : 'Clip renamed.',
    );

  const setClipTags = (id: number, tags: string[]) =>
    runClipAction(() => source.setClipTags(id, tags), 'Tags updated.');

  // The OS actions change nothing in the library, so they neither hold the controls nor
  // refresh; a failure is shown like any other command's.
  async function osAction(action: () => Promise<unknown>, done: string | null) {
    try {
      await action();
      if (done !== null) notice = done;
    } catch (err) {
      error = errorMessage(err);
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
      view = openClip(written.id);
      void loadThumbnails();
    } catch (err) {
      error = errorMessage(err);
    } finally {
      busy = false;
    }
  }

  function clipName(id: number): string {
    return clips.find((clip) => clip.id === id)?.displayName ?? `clip #${id}`;
  }

  /**
   * Save edited settings.
   *
   * The outcome carries the settings read back after the write, so the panel
   * refreshes from it; the library is refreshed too, because a clips-directory move
   * changes what the storage panel reports.
   */
  async function updateSettings(update: SettingsUpdate) {
    busy = true;
    try {
      const outcome = await source.updateSettings(update);
      notice = describeSettingsUpdate(outcome);
      await refresh();
      await refreshSettings();
    } catch (err) {
      error = errorMessage(err);
    } finally {
      busy = false;
    }
  }

  /**
   * Open a session in the sessions view's detail pane.
   *
   * The markers are read here rather than with the list, because they are per session and
   * nothing draws them until one is selected. An empty list is a real answer; a failure is
   * reported and the timeline simply has no markers, which is the same thing the user would
   * see for an untagged session.
   */
  async function selectSession(id: number) {
    view = openSession(id);
    sessionEvents = [];
    sessionMatches = [];
    try {
      sessionEvents = await source.sessionEvents(id);
      sessionMatches = await source.sessionMatches(id);
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

  async function extractMatch(index: number) {
    const session = selectedSession;
    if (session === null) return;
    busy = true;
    try {
      const written = await source.extractMatch(session.id, index);
      notice = describeExtraction(session.title, written);
      await refresh();
      void loadThumbnails();
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
    // The settings panel's values; re-read after every apply, which returns them anyway.
    void refreshSettings();
    void refreshHealth();
    void refreshGsiTargets();
    const poll = setInterval(() => void refreshRecording(), RECORDING_POLL_MS);
    const healthPoll = setInterval(() => void refreshHealth(), HEALTH_POLL_MS);
    return () => {
      clearInterval(poll);
      clearInterval(healthPoll);
    };
  });
</script>

<div class="shell">
  <NavRail
    active={railOf(view)}
    healthAttention={healthBadge(health)}
    onNavigate={(rail: RailView) => (view = navigate(view, rail))}
  />

  <div class="content">
    <RecorderBar
      status={recording}
      {app}
      busy={recordingBusy}
      onStart={startRecording}
      onStop={stopRecording}
      onClip={saveClip}
      onOpenHealth={() => (view = navigate(view, 'health'))}
    />

    <main class="view">
      {#if view.kind === 'library'}
        <section class="library" data-region="library">
          <div class="view-head">
            <h2>Library</h2>
            <span class="muted">
              {isFiltering(filter) ? `${visibleClips.length} of ${clips.length}` : clips.length} indexed
            </span>
          </div>
          <ClipFilterBar
            {filter}
            games={gameOptions(clips)}
            tags={tagOptions(clips)}
            onChange={(next) => (filter = next)}
          />
          <div class="grid" data-region="clip-grid">
            <ClipList
              clips={visibleClips}
              filtering={isFiltering(filter)}
              {thumbnails}
              {selectedId}
              onSelect={(id) => (view = openClip(id))}
              onFavourite={toggleFavourite}
              onDelete={(id) => deleteClip(id, clipName(id))}
              onCopy={(id) =>
                osAction(() => source.copyClipToClipboard(id), 'Clip file copied to the clipboard.')}
              onReveal={(id) => osAction(() => source.revealClip(id), null)}
              onDragOut={(id) => osAction(() => source.dragClipOut(id), null)}
            />
          </div>
        </section>
      {:else if view.kind === 'clip'}
        <section class="page" data-region="clip-page">
          <div class="crumbs">
            <button type="button" class="ghost back" onclick={() => (view = back(view))}>
              <Icon name="chevron-left" size={16} /> Library
            </button>
          </div>
          {#if selected !== null}
            {@const clip = selected}
            {#key clip.id}
              <ClipDetail
                clip={clip}
                {busy}
                onTrim={(range) => trimClip(clip.id, range)}
                onFavourite={(favourite) => toggleFavourite(clip.id, favourite)}
                onRename={(title) => renameClip(clip.id, title)}
                onTags={(tags) => setClipTags(clip.id, tags)}
                onCopy={() =>
                  osAction(() => source.copyClipToClipboard(clip.id), 'Clip file copied to the clipboard.')}
                onReveal={() => osAction(() => source.revealClip(clip.id), null)}
              />
            {/key}
          {/if}
        </section>
      {:else if view.kind === 'sessions' || view.kind === 'session'}
        <section class="sessions" data-region="sessions">
          <div class="master" data-region="session-list">
            <div class="view-head">
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
          </div>
          <div class="detail" data-region="session-page">
            {#if selectedSession !== null}
              {@const session = selectedSession}
              {#key session.id}
                <SessionDetail
                  {session}
                  events={sessionEvents}
                  matches={sessionMatches}
                  onExtractMatch={extractMatch}
                  {busy}
                  onExtract={extractClip}
                  onFavourite={(favourite) => toggleSessionFavourite(session.id, favourite)}
                  onDelete={() => deleteSession(session.id)}
                />
              {/key}
            {:else if loaded}
              <div class="empty-state">
                <p>Select a session to see its markers and cut a clip out of it.</p>
              </div>
            {/if}
          </div>
        </section>
      {:else if view.kind === 'storage'}
        <section class="page" data-region="storage">
          {#if stats !== null}
            <StoragePanel {stats} />
          {/if}
        </section>
      {:else if view.kind === 'health'}
        <section class="page" data-region="health">
          {#if health !== null}
            <HealthPanel {health} />
          {/if}
        </section>
      {:else if view.kind === 'settings'}
        <section class="page" data-region="settings">
          {#if settings !== null}
            <SettingsPanel
              {gsiTargets}
              onInstallGsi={installGsi}
              {settings}
              {recording}
              {busy}
              onApply={updateSettings}
            />
          {/if}
        </section>
      {/if}
    </main>
  </div>

  <Toasts
    {error}
    {notice}
    onDismissError={() => (error = null)}
    onDismissNotice={() => (notice = null)}
  />
</div>

<style>
  .back {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    padding: 0 8px 0 2px;
    font-size: 12px;
    color: var(--text-muted);
  }

  /*
   * A view with nothing selected: one sentence in a pane as tall as the view. Centred and
   * bounded it reads as the pane's state rather than a stray line of text.
   */
  .empty-state {
    display: grid;
    place-items: center;
    flex: 1;
    height: 100%;
    padding: 24px 16px;
  }

  .empty-state p {
    max-width: 44ch;
    text-align: center;
    color: var(--muted);
  }
</style>
