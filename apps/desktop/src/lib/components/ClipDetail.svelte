<script lang="ts">
  /**
   * One clip, with its player and its trim controls.
   *
   * Playback is an HTML5 `<video>` pointed at an `asset:` URL (spec §9) — the Tauri asset
   * protocol, scoped to the clips directory, serving range requests so the element's own
   * scrubbing works. There is no streaming code in this application and there must not be.
   */
  import Timeline from './Timeline.svelte';
  import { parseTagInput } from '../clips';
  import type { ClipView } from '../clips';
  import { formatDateTime } from '../time';
  import { fullRange, rangeLabel, rangeLengthLabel, validateRange } from '../trim';
  import type { TrimRange } from '../types';

  interface Props {
    clip: ClipView;
    /** A command is in flight; both actions are held while it is. */
    busy: boolean;
    onTrim: (range: TrimRange) => void;
    onFavourite: (favourite: boolean) => void;
    onRename: (title: string) => void;
    onTags: (tags: string[]) => void;
    onCopy: () => void;
    onReveal: () => void;
  }

  let { clip, busy, onTrim, onFavourite, onRename, onTags, onCopy, onReveal }: Props = $props();

  // svelte-ignore state_referenced_locally — keyed on the clip id by App (see `range`).
  let titleDraft = $state(clip.title ?? '');
  let tagDraft = $state('');

  const titleChanged = $derived(titleDraft.trim() !== (clip.title ?? ''));

  function saveTitle() {
    if (titleChanged) onRename(titleDraft);
  }

  function addTags() {
    const added = parseTagInput(tagDraft);
    if (added.length === 0) return;
    tagDraft = '';
    onTags([...new Set([...clip.tags, ...added])]);
  }

  let video = $state<HTMLVideoElement | null>(null);
  let playheadMs = $state(0);
  // svelte-ignore state_referenced_locally — deliberate: this component is keyed on the
  // clip id by App, so a different clip remounts it rather than re-rendering it with a new
  // prop. Reading the prop once here is therefore the whole clip, which is what this
  // initial value is meant to be.
  let range = $state<TrimRange>(fullRange(clip.durationMs));
  let playing = $state(false);

  const validation = $derived(validateRange(range, clip.durationMs));

  /**
   * The playhead follows the element, never a timer of this window's own: while the video
   * plays it is read once per animation frame, and `ontimeupdate` covers the frames in
   * between plus every seek and pause. A timer would drift away from the decoder — and the
   * number this produces is the number that gets sent to `trim_clip`, so drift would mean
   * cutting somewhere other than where the user pointed.
   */
  $effect(() => {
    if (!playing) return;
    let frame = 0;
    const tick = () => {
      playheadMs = (video?.currentTime ?? 0) * 1000;
      frame = requestAnimationFrame(tick);
    };
    frame = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(frame);
  });

  function syncPlayhead() {
    playheadMs = (video?.currentTime ?? 0) * 1000;
  }

  function seek(ms: number) {
    playheadMs = ms;
    if (video) video.currentTime = ms / 1000;
  }

  function trim() {
    if (validation.ok) onTrim(validation.range);
  }
</script>

<header class="head">
  <div class="titles">
    <h1>{clip.displayName}</h1>
    <p class="mono muted">{clip.path}</p>
  </div>
  <button
    class="star"
    type="button"
    aria-pressed={clip.favourite}
    title={clip.favourite ? 'Remove from favourites' : 'Protect this clip from cleanup'}
    onclick={() => onFavourite(!clip.favourite)}
    disabled={busy}
  >
    {clip.favourite ? '★ Favourite' : '☆ Favourite'}
  </button>
</header>

<div class="facts">
  {#if clip.game}<span class="chip">{clip.game}</span>{/if}
  <span class="chip">{clip.durationLabel}</span>
  <span class="chip">{clip.sizeLabel}</span>
  <span class="chip">{clip.codec}</span>
  <span class="chip">{formatDateTime(clip.createdAtMs)}</span>
</div>

<section class="meta">
  <form
    class="row"
    onsubmit={(e) => {
      e.preventDefault();
      saveTitle();
    }}
  >
    <input
      type="text"
      aria-label="Clip title"
      placeholder={clip.name}
      maxlength="120"
      bind:value={titleDraft}
      disabled={busy}
    />
    <button type="submit" disabled={busy || !titleChanged}>Rename</button>
  </form>
  <p class="muted note">The title is a label in this library; the file keeps its name.</p>

  <div class="row tagrow">
    {#each clip.tags as tag (tag)}
      <span class="tag">
        #{tag}
        <button
          type="button"
          class="x"
          aria-label={`Remove tag ${tag}`}
          disabled={busy}
          onclick={() => onTags(clip.tags.filter((t) => t !== tag))}>×</button
        >
      </span>
    {/each}
    <form
      class="row"
      onsubmit={(e) => {
        e.preventDefault();
        addTags();
      }}
    >
      <input
        type="text"
        aria-label="Add tags"
        placeholder="Add tags, comma separated"
        maxlength="200"
        bind:value={tagDraft}
        disabled={busy}
      />
      <button type="submit" disabled={busy || parseTagInput(tagDraft).length === 0}>Add</button>
    </form>
  </div>

  <div class="row">
    <button type="button" onclick={onCopy}>Copy file</button>
    <button type="button" onclick={onReveal}>Show in folder</button>
  </div>
</section>

<!--
  svelte-ignore a11y_media_has_caption — there is no caption source to point a <track> at.
  This element plays the user's own gameplay recording, and nothing in this application
  transcribes audio; an empty caption track would satisfy the linter and tell a screen
  reader nothing.
-->
<video
  class="player"
  bind:this={video}
  src={clip.assetUrl}
  controls
  preload="metadata"
  ontimeupdate={syncPlayhead}
  onseeked={syncPlayhead}
  onplay={() => (playing = true)}
  onpause={() => (playing = false)}
  onended={() => (playing = false)}
></video>

<Timeline
  durationMs={clip.durationMs}
  {playheadMs}
  {range}
  onSeek={seek}
  onRangeChange={(next) => (range = next)}
/>

<section class="panel trim">
  <h2>Trim</h2>

  <div class="selection">
    <span class="mono">{rangeLabel(range)}</span>
    <span class="muted">→ {rangeLengthLabel(range)} selected</span>
  </div>

  <div class="actions">
    <button
      class="primary"
      type="button"
      disabled={!validation.ok || busy}
      onclick={trim}
    >
      {busy ? 'Working…' : 'Trim to a new clip'}
    </button>
    <button
      type="button"
      disabled={busy}
      onclick={() => (range = fullRange(clip.durationMs))}
    >
      Select all
    </button>
  </div>

  {#if !validation.ok}
    <p class="invalid">{validation.reason}</p>
  {/if}

  <p class="lossless">
    <strong>Lossless.</strong> The selection is cut with an ffmpeg stream copy
    (<code>-c copy</code>), so nothing is re-encoded — but the cut can only land where the
    stream allows, and the file can differ by a fraction of a second from the selection.
    It is written next to the original as
    <code>{clip.name.replace(/\.[^.]+$/, '')}.trim-START-END.&lt;ext&gt;</code>; the
    original is never modified.
  </p>
</section>

<style>
  .head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
    padding: 14px 16px 8px;
  }

  h1 {
    font-size: 17px;
    word-break: break-all;
  }

  .titles {
    min-width: 0;
  }

  .star[aria-pressed='true'] {
    color: var(--star);
    border-color: var(--star);
  }

  .meta {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 0 16px 12px;
  }

  .row {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 6px;
  }

  .row input {
    flex: 1 1 200px;
    min-width: 0;
  }

  .note {
    margin: 0;
    font-size: 11px;
  }

  .tag {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    font-size: 12px;
    border: 1px solid var(--line);
    border-radius: 10px;
    padding: 1px 4px 1px 8px;
  }

  .x {
    border: 0;
    background: transparent;
    padding: 0 4px;
    line-height: 1;
  }

  .facts {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    padding: 0 16px 12px;
  }

  .player {
    display: block;
    width: calc(100% - 32px);
    margin: 0 16px;
    /*
     * The box is width-driven and capped in height, so the height that *paints* is normally
     * the media's own. `aspect-ratio` is what keeps it there when there is no media yet:
     * an unloaded `<video>` has an intrinsic size of 300x150, and in this column it is a
     * flex item — so without this the pane's flexbox shrinks the player to its 150px
     * intrinsic height until metadata arrives, and a clip whose file cannot be read leaves
     * it that way for good. A 640x360 clip needs 488px at this width; 368 is the cap.
     */
    aspect-ratio: 16 / 9;
    max-height: 46vh;
    background: #000;
    border: 1px solid var(--line);
    border-radius: 6px;
  }

  .selection {
    display: flex;
    align-items: baseline;
    gap: 10px;
    font-size: 13px;
  }

  .actions {
    display: flex;
    gap: 8px;
  }

  .trim {
    border-top: 0;
    padding-top: 0;
  }

  .invalid {
    color: var(--warn);
  }

  .lossless {
    font-size: 12px;
    color: var(--text);
    background: var(--panel-2);
    border: 1px solid var(--accent-dim);
    border-radius: 4px;
    padding: 8px 10px;
  }
</style>
