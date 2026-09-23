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
  import StoragePanel from './lib/components/StoragePanel.svelte';
  import { describeDelete, describeTrim, resolveSelection, thumbnailAtMs, toClipViews } from './lib/clips';
  import type { ClipView } from './lib/clips';
  import { errorCode, errorMessage, tauriIpc } from './lib/ipc';
  import type { ClipSource } from './lib/ipc';
  import type { StorageStats, TrimRange } from './lib/types';

  interface Props {
    /** Injected so the window can be driven without the Tauri runtime. */
    source?: ClipSource;
  }

  let { source = tauriIpc }: Props = $props();

  let clips = $state<ClipView[]>([]);
  let stats = $state<StorageStats | null>(null);
  /** Clip id → `asset:` URL of its disk-cached thumbnail. */
  let thumbnails = $state<Record<number, string>>({});
  let selectedId = $state<number | null>(null);
  /** A command is in flight; the controls that mutate are held while it is. */
  let busy = $state(false);
  let error = $state<string | null>(null);
  let notice = $state<string | null>(null);
  let loaded = $state(false);

  const selected = $derived(clips.find((clip) => clip.id === selectedId) ?? null);

  async function refresh() {
    try {
      const [rows, storage] = await Promise.all([source.listClips(), source.storageStats()]);
      clips = toClipViews(rows, source.assetUrl);
      stats = storage;
      selectedId = resolveSelection(clips, selectedId);
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

  onMount(async () => {
    await refresh();
    void loadThumbnails();
  });
</script>

<div class="shell">
  <aside class="sidebar">
    <div class="pane-head">
      <h2>Clips</h2>
      <span class="muted">{clips.length} indexed</span>
    </div>
    <ClipList
      {clips}
      {thumbnails}
      {selectedId}
      onSelect={(id) => (selectedId = id)}
      onFavourite={toggleFavourite}
      onDelete={(id) => deleteClip(id, clipName(id))}
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

    {#if selected !== null}
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
        Select a clip on the left to play it, scrub it and trim it.
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
</style>
