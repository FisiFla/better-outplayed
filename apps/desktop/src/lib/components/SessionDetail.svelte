<script lang="ts">
  /**
   * One session: what it is, its timeline, and the range to cut a clip out of.
   *
   * This is a **range picker**, not a player. A session file is one long recording and
   * `ClipDetail` is where a clip is played; what a session's timeline is for is choosing the
   * part worth keeping, which is why the markers are drawn here — they are the reason to
   * scrub a session at all, and they arrive measured on the same media clock the range is.
   *
   * The cursor is not playback: it is where the last marker click landed, so the timeline
   * shows what a click did. Nothing here pretends to play, and there is no `<video>` to
   * drive.
   */
  import Timeline from './Timeline.svelte';
  import {
    canExtract,
    describeSession,
    extractionBlocker,
    sessionLengthLabel,
    sessionTitle,
  } from '../sessions';
  import type { SessionView } from '../sessions';
  import { formatBytes, formatPrecise } from '../time';
  import type { SessionEvent, TrimRange } from '../types';

  interface Props {
    session: SessionView;
    /** The session's markers, in the store's media-time order. */
    events: SessionEvent[];
    busy: boolean;
    onExtract: (range: TrimRange) => void;
    onFavourite: (favourite: boolean) => void;
    onDelete: () => void;
  }

  let { session, events, busy, onExtract, onFavourite, onDelete }: Props = $props();

  /**
   * The range to cut out. The whole session until the user says otherwise.
   *
   * Seeded empty and set in the effect below rather than in this initialiser, which would
   * capture the session's length as it was on the first render and never notice it change —
   * and it does change: a session's length is only known once its segments have been
   * concatenated, so a view watching a recording finish sees it go from unknown to real.
   */
  let range = $state<TrimRange>({ startMs: 0, endMs: 0 });
  /** Where the last marker click put the cursor. Not playback — see the header note. */
  let cursorMs = $state(0);

  const blocker = $derived(extractionBlocker(session));
  const extractable = $derived(canExtract(session));

  // A change of axis resets both, so neither can be left sitting past the end of a shorter
  // recording. Reads only the session's length, so it cannot re-trigger itself.
  $effect(() => {
    const length = session.durationMs;
    range = { startMs: 0, endMs: length };
    cursorMs = 0;
  });
</script>

<section class="detail">
  <header>
    <h2>{sessionTitle(session)}</h2>
    <span class="muted">{describeSession(session)}</span>
  </header>

  <dl class="facts">
    <div><dt>Mode</dt><dd class="mono">{session.mode}</dd></div>
    <div><dt>Media length</dt><dd class="mono">{sessionLengthLabel(session)}</dd></div>
    <div><dt>On disk</dt><dd class="mono">{formatBytes(session.sizeBytes)}</dd></div>
    <div><dt>Markers</dt><dd class="mono">{events.length}</dd></div>
    <div class="wide"><dt>Segments</dt><dd class="mono path">{session.scratchDir}</dd></div>
  </dl>

  {#if session.hasLength}
    <Timeline
      durationMs={session.durationMs}
      playheadMs={cursorMs}
      {range}
      markers={events}
      onSeek={(ms) => (cursorMs = ms)}
      onRangeChange={(next) => (range = next)}
    />
  {:else}
    <p class="empty">
      This session's media length is not known, so there is no timeline to draw. That is the
      case while it is still recording, and for a session recovered after a crash.
    </p>
  {/if}

  {#if events.length > 0}
    <table class="markers">
      <caption class="muted">Every marker on this session, in media time.</caption>
      <thead>
        <tr><th>Marker</th><th>At</th><th>Clip</th></tr>
      </thead>
      <tbody>
        {#each events as event (event.id)}
          <tr>
            <td>{event.kind}</td>
            <td class="mono">{formatPrecise(event.offset_ms)}</td>
            <td class="mono muted">{event.clip_id === null ? '—' : `#${event.clip_id}`}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}

  <div class="controls">
    <button
      type="button"
      class="primary"
      disabled={!extractable || busy || range.endMs <= range.startMs}
      onclick={() => onExtract(range)}
    >
      {busy ? 'Working…' : 'Extract clip'}
    </button>
    <span class="muted">
      {formatPrecise(range.startMs)}–{formatPrecise(range.endMs)}
      ({formatPrecise(range.endMs - range.startMs)})
    </span>
    <button type="button" onclick={() => onFavourite(!session.favourite)}>
      {session.favourite ? 'Un-favourite' : 'Favourite'}
    </button>
    <button type="button" class="danger" onclick={onDelete}>Delete session</button>
  </div>

  {#if blocker !== null}
    <p class="blocked">{blocker}</p>
  {:else}
    <p class="hint muted">
      The cut is lossless (<code class="mono">-c copy</code>), so it snaps to the nearest
      keyframe before the start rather than re-encoding: the clip may begin slightly earlier
      than the range, and it is never re-encoded.
    </p>
  {/if}
</section>

<style>
  .detail {
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 14px 16px 18px;
    overflow-y: auto;
  }

  header {
    display: flex;
    align-items: baseline;
    gap: 12px;
  }

  h2 {
    margin: 0;
    font-size: 15px;
  }

  .facts {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(140px, 1fr));
    gap: 8px 16px;
    margin: 0;
    font-size: 12px;
  }

  .facts div {
    display: flex;
    flex-direction: column;
    gap: 1px;
  }

  .facts dt {
    color: var(--muted);
    font-size: 11px;
  }

  .facts dd {
    margin: 0;
  }

  .path {
    overflow-wrap: anywhere;
  }

  .markers {
    width: 100%;
    border-collapse: collapse;
    font-size: 12px;
  }

  .markers caption {
    text-align: left;
    padding-bottom: 4px;
    font-size: 11px;
  }

  .markers th,
  .markers td {
    text-align: left;
    padding: 3px 8px 3px 0;
    border-bottom: 1px solid var(--line);
  }

  .controls {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }

  .primary {
    font-weight: 600;
  }

  .controls > .danger {
    margin-left: auto;
  }

  .blocked {
    margin: 0;
    padding: 8px 10px;
    border: 1px solid var(--line);
    border-radius: 5px;
    background: var(--panel-2);
    font-size: 12px;
  }

  .hint {
    margin: 0;
    font-size: 11px;
  }
</style>
