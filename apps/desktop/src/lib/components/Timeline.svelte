<script lang="ts">
  /**
   * The timeline scrubber.
   *
   * Four things live on the track: the playhead (driven by the `<video>` element in
   * `ClipDetail`), two trim handles, and — when the clip came from a session — that session's
   * event markers. The arithmetic behind all of it is in `../trim.ts` and `../markers.ts`,
   * both tested without a DOM; this component only turns pointer positions into milliseconds
   * and draws the result.
   *
   * Markers used to be forbidden here, on the grounds that the `events` table existed but
   * nothing wrote to it: a timeline drawn from an always-empty table would look like a feature
   * and tell the user nothing. That is no longer true in either half. The recorder links every
   * event it records to the session that was running and a hotkey clip writes a `bookmark`,
   * and a session's offsets are measured on the media clock the store computes from its
   * `media_epoch_ms`. So markers are drawn — and `markers` defaults to `[]`, so a clip with no
   * session behind it draws exactly what it drew before.
   */
  import {
    KEYBOARD_COARSE_STEP_MS,
    KEYBOARD_STEP_MS,
    dragHandle,
    fractionToMs,
    msToPercent,
    nudgeHandle,
    rangeLengthLabel,
    rangeLabel,
    rangePercents,
  } from '../trim';
  import { isClick, placeMarkers, seekTargetMs } from '../markers';
  import { formatPrecise } from '../time';
  import type { SessionEvent, TrimRange } from '../types';

  interface Props {
    durationMs: number;
    playheadMs: number;
    range: TrimRange;
    /** Move playback to `ms`. */
    onSeek: (ms: number) => void;
    onRangeChange: (range: TrimRange) => void;
    /**
     * The markers of the session this clip came from, in the store's media-time order.
     * Empty for a clip with no session behind it — which is the default, because `events` rows
     * belong to a session and a clip is not a session.
     */
    markers?: SessionEvent[];
  }

  let { durationMs, playheadMs, range, onSeek, onRangeChange, markers = [] }: Props = $props();

  let track = $state<HTMLDivElement | null>(null);
  /** What the pointer is currently moving, if anything. */
  let dragging = $state<'start' | 'end' | 'playhead' | null>(null);
  /** Where the current gesture started, so a marker click can be told from a scrub. */
  let gestureStart = $state<{ x: number; y: number } | null>(null);
  /** The marker the current gesture began on, if it began on one. */
  let pressedMarker = $state<SessionEvent | null>(null);

  const percents = $derived(rangePercents(range, durationMs));
  const playheadPercent = $derived(msToPercent(playheadMs, durationMs));
  const placed = $derived(placeMarkers(markers, durationMs));
  const disabled = $derived(durationMs <= 0);

  /** The millisecond offset under the pointer. */
  function msAt(event: PointerEvent): number {
    const element = track;
    if (!element) return 0;
    const rect = element.getBoundingClientRect();
    if (rect.width <= 0) return 0;
    return fractionToMs((event.clientX - rect.left) / rect.width, durationMs);
  }

  function apply(kind: 'start' | 'end' | 'playhead', event: PointerEvent) {
    const ms = msAt(event);
    if (kind === 'playhead') {
      onSeek(ms);
      return;
    }
    onRangeChange(dragHandle(range, kind, ms, durationMs));
  }

  function begin(kind: 'start' | 'end' | 'playhead', event: PointerEvent) {
    if (disabled) return;
    event.preventDefault();
    dragging = kind;
    // Capture on the track, not on the handle: a drag that leaves the handle — which every
    // drag past the opposite handle does — must keep being delivered here.
    track?.setPointerCapture(event.pointerId);
    apply(kind, event);
  }

  /**
   * Begin a gesture on the track itself: a scrub, or — if it started on a marker — a press
   * that may turn out to be a click. Which one it was is decided on release, by distance.
   */
  function beginPlayhead(event: PointerEvent, marker: SessionEvent | null) {
    if (disabled) return;
    gestureStart = { x: event.clientX, y: event.clientY };
    pressedMarker = marker;
    begin('playhead', event);
  }

  function onPointerMove(event: PointerEvent) {
    if (dragging !== null) apply(dragging, event);
  }

  function onPointerUp(event: PointerEvent) {
    if (dragging === null) return;
    const start = gestureStart;
    const marker = pressedMarker;
    dragging = null;
    gestureStart = null;
    pressedMarker = null;
    if (track?.hasPointerCapture(event.pointerId)) {
      track.releasePointerCapture(event.pointerId);
    }
    // A press that did not travel is a click: on a marker it seeks to that marker's lead-in,
    // not to the pixel under the cursor. Without this the click would have scrubbed to the
    // pointer and left the marker — the thing the user aimed at — unused.
    if (marker !== null && start !== null && isClick(start, { x: event.clientX, y: event.clientY })) {
      onSeek(seekTargetMs(marker.offset_ms));
    }
  }

  function onHandleKey(handle: 'start' | 'end', event: KeyboardEvent) {
    const step = event.shiftKey ? KEYBOARD_COARSE_STEP_MS : KEYBOARD_STEP_MS;
    if (event.key === 'ArrowLeft') {
      onRangeChange(nudgeHandle(range, handle, -step, durationMs));
    } else if (event.key === 'ArrowRight') {
      onRangeChange(nudgeHandle(range, handle, step, durationMs));
    } else {
      return;
    }
    // Arrow keys scroll the pane otherwise.
    event.preventDefault();
  }
</script>

<div class="timeline">
  <div class="legend">
    <span class="mono">{rangeLabel(range)}</span>
    <span class="muted">selection {rangeLengthLabel(range)}</span>
    <span class="playhead-readout mono">{formatPrecise(playheadMs)}</span>
  </div>

  <div
    class="track"
    bind:this={track}
    onpointerdown={(event) => beginPlayhead(event, null)}
    onpointermove={onPointerMove}
    onpointerup={onPointerUp}
    onpointercancel={onPointerUp}
    role="none"
  >
    <div class="selection" style="left: {percents.left}%; width: {percents.width}%"></div>

    {#each placed as marker (marker.event.id)}
      <button
        class="marker"
        style="left: {marker.percent}%; --marker: {marker.color}"
        type="button"
        title="{marker.label} at {formatPrecise(marker.event.offset_ms)}"
        aria-label="Marker: {marker.label} at {formatPrecise(marker.event.offset_ms)}"
        disabled={disabled}
        onpointerdown={(event) => {
          // Deliberately NOT stopped: the track still begins its drag, so a drag that starts
          // on a marker scrubs like any other. `onPointerUp` tells the two apart by distance,
          // which is what makes a marker both clickable and draggable past.
          beginPlayhead(event, marker.event);
        }}
      ></button>
    {/each}

    <button
      class="handle start"
      style="left: {percents.left}%"
      type="button"
      role="slider"
      aria-label="Trim start"
      aria-valuemin={0}
      aria-valuemax={durationMs}
      aria-valuenow={range.startMs}
      aria-valuetext={formatPrecise(range.startMs)}
      disabled={disabled}
      onpointerdown={(event) => {
        event.stopPropagation();
        begin('start', event);
      }}
      onkeydown={(event) => onHandleKey('start', event)}
    ></button>

    <button
      class="handle end"
      style="left: {percents.left + percents.width}%"
      type="button"
      role="slider"
      aria-label="Trim end"
      aria-valuemin={0}
      aria-valuemax={durationMs}
      aria-valuenow={range.endMs}
      aria-valuetext={formatPrecise(range.endMs)}
      disabled={disabled}
      onpointerdown={(event) => {
        event.stopPropagation();
        begin('end', event);
      }}
      onkeydown={(event) => onHandleKey('end', event)}
    ></button>

    <div class="playhead" style="left: {playheadPercent}%"></div>
  </div>

  <p class="hint muted">
    Drag the track to scrub. Drag the two handles to set the trim range; focus one and use
    the arrow keys for a {KEYBOARD_STEP_MS}ms step, or Shift for {KEYBOARD_COARSE_STEP_MS}ms.
    {#if placed.length > 0}
      Click a marker to seek to just before it ({placed.length} on this session's timeline).
    {/if}
  </p>
</div>

<style>
  .timeline {
    display: flex;
    flex-direction: column;
    gap: 7px;
    padding: 12px 16px 14px;
    border-top: 1px solid var(--line);
    background: var(--panel);
  }

  .legend {
    display: flex;
    align-items: baseline;
    gap: 12px;
    font-size: 12px;
  }

  .playhead-readout {
    margin-left: auto;
    color: var(--accent);
  }

  .track {
    position: relative;
    height: 46px;
    border: 1px solid var(--line);
    border-radius: 6px;
    background: linear-gradient(var(--panel-2), var(--panel-2));
    cursor: crosshair;
    touch-action: none;
  }

  .selection {
    position: absolute;
    top: 0;
    bottom: 0;
    background: rgba(94, 234, 212, 0.16);
    border-left: 1px solid rgba(94, 234, 212, 0.5);
    border-right: 1px solid rgba(94, 234, 212, 0.5);
    pointer-events: none;
  }

  .playhead {
    position: absolute;
    top: -2px;
    bottom: -2px;
    width: 2px;
    margin-left: -1px;
    background: var(--accent);
    box-shadow: 0 0 6px rgba(94, 234, 212, 0.6);
    pointer-events: none;
  }

  /*
   * A marker is a press target inside the track, so it is painted narrow and hit wide: the
   * button spans the full track height and 9px across, while the visible tick is 3px. Two
   * markers a second apart in a long session would otherwise be a pixel apart and impossible
   * to click at all.
   */
  .marker {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 9px;
    margin-left: -4.5px;
    padding: 0;
    border: 0;
    background: transparent;
    cursor: pointer;
  }

  .marker::before {
    content: '';
    position: absolute;
    left: 3px;
    top: 3px;
    width: 3px;
    height: 13px;
    border-radius: 1px;
    background: var(--marker, #94a3b8);
  }

  .marker:hover::before,
  .marker:focus-visible::before {
    box-shadow: 0 0 0 3px rgba(148, 163, 184, 0.35);
  }

  .marker:focus-visible {
    outline: none;
  }

  .handle {
    position: absolute;
    top: -4px;
    bottom: -4px;
    width: 14px;
    margin-left: -7px;
    padding: 0;
    border: 0;
    background: transparent;
    cursor: ew-resize;
  }

  /* The grab bar itself, so the hit area can be wider than what is painted. */
  .handle::before {
    content: '';
    position: absolute;
    left: 5px;
    top: 0;
    bottom: 0;
    width: 4px;
    border-radius: 2px;
    background: var(--accent);
  }

  .handle:hover::before,
  .handle:focus-visible::before {
    box-shadow: 0 0 0 3px rgba(94, 234, 212, 0.35);
  }

  .handle:focus-visible {
    outline: none;
  }

  .hint {
    font-size: 11px;
  }
</style>
