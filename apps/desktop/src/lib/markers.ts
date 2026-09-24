/**
 * The arithmetic and the vocabulary behind a session's timeline markers.
 *
 * Kept out of the component for the same reason `trim.ts` is: this is the part that can be
 * wrong in a way no screenshot would show — an offset plotted from the wrong clock, a colour
 * that hides a marker, a click that seeks somewhere the user did not ask for — and it is
 * testable without a DOM.
 *
 * The offsets arrive from the store already measured in **media time from the session's
 * start**. Nothing here converts between clocks and nothing here may: the store computes
 * `events.at - sessions.media_epoch_ms` in one SQL expression precisely so that this layer
 * cannot disagree with it. A wall clock reaching this file would be the cross-clock defect
 * again, one layer further out.
 */

import type { SessionEvent } from './types';

/**
 * How far *before* a marker a click seeks.
 *
 * This is the point of the feature. Seeking exactly to a marker puts the moment that caused
 * it on the very first frame, so pressing play shows what happened *after* it; seeking a
 * little before is what makes a marker usable for review.
 */
export const MARKER_LEAD_MS = 3_000;

/**
 * The colour each tag is drawn in.
 *
 * A tag that is not in here is **not** hidden and **not** an error — it is drawn in the
 * neutral colour. `localplay-store` passes `events.kind` through verbatim and knows no
 * vocabulary (spec §5.5), so a tag this build has never heard of is a marker that exists.
 * A palette that threw, or one that skipped unknown tags, would let an unrecognised game
 * integration silently delete its own timeline.
 */
const KIND_COLORS: Record<string, string> = {
  /** The user's own marks, in the clip accent — a bookmark is what produces a clip. */
  bookmark: 'var(--accent)',
  /** League highlights. */
  kill: '#f97316',
  death: '#ef4444',
  assist: '#eab308',
  /** Round and match boundaries: markers rather than highlights. */
  round_start: '#38bdf8',
  round_end: '#38bdf8',
  bomb_planted: '#f97316',
  bomb_defused: '#38bdf8',
  match_start: '#a78bfa',
  match_end: '#a78bfa',
};

/** The colour for a tag this build does not know. Deliberately visible, not a hairline. */
export const NEUTRAL_MARKER_COLOR = '#94a3b8';

/**
 * A readable label for a tag.
 *
 * An unknown tag is shown as itself with its underscores opened out, rather than as
 * "unknown": the tag came from an integration, and naming it verbatim is what lets someone
 * report "what is a `bomb_planted`" instead of losing the information entirely.
 */
const KIND_LABELS: Record<string, string> = {
  bookmark: 'bookmark',
  kill: 'kill',
  death: 'death',
  assist: 'assist',
  round_start: 'round start',
  round_end: 'round end',
  bomb_planted: 'bomb planted',
  bomb_defused: 'bomb defused',
  match_start: 'match start',
  match_end: 'match end',
};

export function markerColor(kind: string): string {
  return KIND_COLORS[kind] ?? NEUTRAL_MARKER_COLOR;
}

export function markerLabel(kind: string): string {
  return KIND_LABELS[kind] ?? kind.replace(/_/g, ' ');
}

/** Whether this build has a colour and a name for the tag. */
export function isKnownKind(kind: string): boolean {
  return kind in KIND_COLORS;
}

/**
 * Where a marker sits on the track, as a percentage from 0 to 100.
 *
 * Clamped rather than dropped: a marker past the end of the clip's duration is still a real
 * marker — a clip cut out of a session carries the session's offsets, so a marker outside the
 * clip's own window should read as "at the far edge", not be silently absent. A duration of 0
 * or less has no axis at all, and every marker collapses onto 0 rather than dividing by zero
 * and poisoning the whole track with `NaN`.
 */
export function markerPercent(offsetMs: number, durationMs: number): number {
  if (!(durationMs > 0) || !Number.isFinite(offsetMs)) return 0;
  const percent = (offsetMs / durationMs) * 100;
  return Number.isFinite(percent) ? Math.min(100, Math.max(0, percent)) : 0;
}

/**
 * Where clicking a marker seeks to: its own instant minus the lead, never before the start.
 *
 * Clamped at 0 because a negative `currentTime` is ignored by the media element — an early
 * marker would then look like a dead button rather than a seek to the beginning.
 */
export function seekTargetMs(offsetMs: number, leadMs: number = MARKER_LEAD_MS): number {
  if (!Number.isFinite(offsetMs)) return 0;
  return Math.max(0, offsetMs - leadMs);
}

/**
 * How far the pointer may move and still count as a click rather than a drag, in pixels.
 *
 * A marker sits *inside* the track, so pressing one also starts the track's own playhead drag.
 * Without a threshold, every marker click would scrub to the pointer's position *and* seek to
 * the marker, and the two would fight. A few pixels of slop is the standard answer: below it
 * the gesture is a click, above it the user is scrubbing and the marker merely had the
 * misfortune of being under the first pixel.
 */
export const CLICK_SLOP_PX = 4;

/** Whether a gesture that went down at `down` and up at `up` was a click, not a drag. */
export function isClick(
  down: { x: number; y: number },
  up: { x: number; y: number },
  slopPx: number = CLICK_SLOP_PX,
): boolean {
  return Math.abs(up.x - down.x) <= slopPx && Math.abs(up.y - down.y) <= slopPx;
}

/** A marker with everything the track needs to draw it, computed once. */
export interface PlacedMarker {
  event: SessionEvent;
  /** Percentage from the left edge of the track. */
  percent: number;
  color: string;
  label: string;
}

/**
 * Place a session's markers on a track that is `durationMs` long.
 *
 * Input order is preserved. The store returns markers in media-time order and this does not
 * re-sort: two markers at the same instant are two markers, and their order is the store's
 * answer (it breaks the tie by row id), not this layer's.
 */
export function placeMarkers(events: SessionEvent[], durationMs: number): PlacedMarker[] {
  return events.map((event) => ({
    event,
    percent: markerPercent(event.offset_ms, durationMs),
    color: markerColor(event.kind),
    label: markerLabel(event.kind),
  }));
}
