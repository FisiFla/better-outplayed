<script lang="ts">
  /**
   * The timeline scrubber.
   *
   * Three things live on the track: the playhead (driven by the `<video>` element in
   * `ClipDetail`), and two trim handles. The arithmetic behind all of it is in
   * `../trim.ts`, which is tested without a DOM; this component only turns pointer
   * positions into milliseconds and draws the result.
   *
   * THERE ARE NO EVENT MARKERS HERE, AND THERE MUST NOT BE. Spec §9 describes a timeline
   * "with event markers", rendered from the `events` table. That table exists in the schema
   * (spec §5.5) but nothing writes to it: the game-event integrations are Phase 4, and no
   * Phase 1 code path inserts a row. Drawing markers from a table that is always empty
   * would look like a feature and tell the user nothing, so the timeline draws clips and
   * nothing else until there is data to draw.
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
  import { formatPrecise } from '../time';
  import type { TrimRange } from '../types';

  interface Props {
    durationMs: number;
    playheadMs: number;
    range: TrimRange;
    /** Move playback to `ms`. */
    onSeek: (ms: number) => void;
    onRangeChange: (range: TrimRange) => void;
  }

  let { durationMs, playheadMs, range, onSeek, onRangeChange }: Props = $props();

  let track = $state<HTMLDivElement | null>(null);
  /** What the pointer is currently moving, if anything. */
  let dragging = $state<'start' | 'end' | 'playhead' | null>(null);

  const percents = $derived(rangePercents(range, durationMs));
  const playheadPercent = $derived(msToPercent(playheadMs, durationMs));
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

  function onPointerMove(event: PointerEvent) {
    if (dragging !== null) apply(dragging, event);
  }

  function onPointerUp(event: PointerEvent) {
    if (dragging === null) return;
    dragging = null;
    if (track?.hasPointerCapture(event.pointerId)) {
      track.releasePointerCapture(event.pointerId);
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
    onpointerdown={(event) => begin('playhead', event)}
    onpointermove={onPointerMove}
    onpointerup={onPointerUp}
    onpointercancel={onPointerUp}
    role="none"
  >
    <div class="selection" style="left: {percents.left}%; width: {percents.width}%"></div>

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
