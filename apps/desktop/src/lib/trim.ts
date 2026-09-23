/**
 * The trim range: clamping, validation and the timeline geometry, as pure arithmetic.
 *
 * This is the module the scrubber's pointer handling and the trim button are both built
 * on, so the *rules* live here — where they are tested without a DOM, a window or a video
 * element — and the components only do the geometry of turning a pointer position into a
 * millisecond offset.
 *
 * The authority on whether a range is acceptable is the Rust side: `trim_clip` rejects an
 * empty or inverted range and one that runs past the end of the clip, with a message that
 * names the numbers. This module exists so the UI cannot send one in the first place, and
 * [`validateRange`] is the same rule set expressed for the UI rather than a second source
 * of truth about what ffmpeg will do.
 */

import { clamp, formatDuration, formatPrecise } from './time';
import type { TrimRange } from './types';

/**
 * The shortest selection the UI will offer.
 *
 * Not a limit ffmpeg imposes — a stream copy could be handed a one-millisecond range — but
 * below this the two handles overlap, the readout stops being readable, and the result is a
 * clip nobody can scrub. It is a UI floor and is named as one.
 */
export const MIN_TRIM_MS = 100;

/** The distance one arrow-key press moves a handle. */
export const KEYBOARD_STEP_MS = 100;

/** The distance one arrow-key press moves a handle when Shift is held. */
export const KEYBOARD_COARSE_STEP_MS = 1_000;

function wholeMs(value: number): number {
  if (!Number.isFinite(value)) return 0;
  return Math.round(value);
}

/**
 * The range the clip's duration can actually hold.
 *
 * The end bound is inclusive: `end = duration` is a legal range — that is what a scrubber
 * dragged to the right-hand edge asks for — and it is exactly the bound `trim_clip`
 * accepts.
 */
export function clampRange(range: TrimRange, durationMs: number): TrimRange {
  const duration = Math.max(0, wholeMs(durationMs));
  if (duration === 0) return { startMs: 0, endMs: 0 };

  const min = Math.min(MIN_TRIM_MS, duration);
  let start = clamp(wholeMs(range.startMs), 0, duration);
  let end = clamp(wholeMs(range.endMs), 0, duration);

  if (end - start < min) {
    // The caller asked for a range that is too short or inverted. Grow it from the start
    // it named, and only if that would run off the end of the clip, slide it back so the
    // selection stays inside the clip.
    if (start + min <= duration) {
      end = start + min;
    } else {
      start = duration - min;
      end = duration;
    }
  }
  return { startMs: start, endMs: end };
}

/** A range that covers the whole clip. */
export function fullRange(durationMs: number): TrimRange {
  return clampRange({ startMs: 0, endMs: durationMs }, durationMs);
}

/**
 * Move one handle to `ms`, keeping the other where it is.
 *
 * Only the dragged handle moves: the selection cannot be pushed around by the handle that
 * is not under the pointer, and a handle cannot cross the other — an inverted range is not
 * something the UI ever produces, because the Rust side would reject it and the user would
 * see an error for a drag that looked legal.
 */
export function dragHandle(
  range: TrimRange,
  handle: 'start' | 'end',
  ms: number,
  durationMs: number,
): TrimRange {
  const duration = Math.max(0, wholeMs(durationMs));
  if (duration === 0) return { startMs: 0, endMs: 0 };

  const min = Math.min(MIN_TRIM_MS, duration);
  const current = clampRange(range, duration);
  const at = clamp(wholeMs(ms), 0, duration);

  if (handle === 'start') {
    const start = clamp(at, 0, Math.max(0, current.endMs - min));
    return { startMs: start, endMs: current.endMs };
  }
  const end = clamp(at, Math.min(duration, current.startMs + min), duration);
  return { startMs: current.startMs, endMs: end };
}

/** Move one handle by a fixed number of milliseconds, for the keyboard. */
export function nudgeHandle(
  range: TrimRange,
  handle: 'start' | 'end',
  deltaMs: number,
  durationMs: number,
): TrimRange {
  const current = clampRange(range, durationMs);
  const from = handle === 'start' ? current.startMs : current.endMs;
  return dragHandle(current, handle, from + deltaMs, durationMs);
}

export type RangeValidation =
  | { ok: true; range: TrimRange }
  | { ok: false; reason: string };

/**
 * Whether the range can be sent to `trim_clip`, and if not, why — phrased for the user.
 *
 * The two refusal cases mirror the two the Rust command refuses, in the same order: an
 * empty range first, because that is the one a user can produce by dragging one handle onto
 * the other, then a range that runs past the end of the clip.
 */
export function validateRange(range: TrimRange, durationMs: number): RangeValidation {
  const duration = Math.max(0, wholeMs(durationMs));
  if (duration === 0) {
    return { ok: false, reason: 'This clip has no duration to trim.' };
  }

  const start = wholeMs(range.startMs);
  const end = wholeMs(range.endMs);

  if (end <= start) {
    return {
      ok: false,
      reason: `The selection is empty: ${formatPrecise(start)} to ${formatPrecise(end)}.`,
    };
  }
  if (end > duration) {
    return {
      ok: false,
      reason: `The selection ends at ${formatPrecise(end)} but the clip is ${formatDuration(duration)} long.`,
    };
  }
  if (end - start < MIN_TRIM_MS) {
    return {
      ok: false,
      reason: `The shortest selection this window will cut is ${MIN_TRIM_MS}ms.`,
    };
  }
  return { ok: true, range: { startMs: start, endMs: end } };
}

/** `0:02.000 → 0:05.000`, the live readout beside the scrubber. */
export function rangeLabel(range: TrimRange): string {
  return `${formatPrecise(range.startMs)} → ${formatPrecise(range.endMs)}`;
}

/** The length of the selection, at the same precision as a clip's duration. */
export function rangeLengthLabel(range: TrimRange): string {
  return formatDuration(Math.max(0, range.endMs - range.startMs));
}

/** A timeline position as a percentage of the clip, for the playhead. */
export function msToPercent(ms: number, durationMs: number): number {
  const duration = wholeMs(durationMs);
  if (duration <= 0) return 0;
  return clamp((wholeMs(ms) / duration) * 100, 0, 100);
}

/** A fraction of the track (0..1) as a timeline position. */
export function fractionToMs(fraction: number, durationMs: number): number {
  const duration = Math.max(0, wholeMs(durationMs));
  if (duration === 0) return 0;
  return clamp(Math.round(clamp(fraction, 0, 1) * duration), 0, duration);
}

/** The selection as a left edge and a width, both percentages of the clip. */
export function rangePercents(
  range: TrimRange,
  durationMs: number,
): { left: number; width: number } {
  const duration = wholeMs(durationMs);
  if (duration <= 0) return { left: 0, width: 0 };
  const clamped = clampRange(range, duration);
  return {
    left: msToPercent(clamped.startMs, duration),
    width: msToPercent(clamped.endMs - clamped.startMs, duration),
  };
}
