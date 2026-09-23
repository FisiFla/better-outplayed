import { describe, expect, it } from 'vitest';
import {
  KEYBOARD_STEP_MS,
  MIN_TRIM_MS,
  clampRange,
  dragHandle,
  fractionToMs,
  fullRange,
  msToPercent,
  nudgeHandle,
  rangeLabel,
  rangeLengthLabel,
  rangePercents,
  validateRange,
} from './trim';

describe('clampRange', () => {
  it('keeps a sane range exactly where it is', () => {
    expect(clampRange({ startMs: 1_000, endMs: 3_000 }, 4_000)).toEqual({
      startMs: 1_000,
      endMs: 3_000,
    });
  });

  it('pulls a range that runs off either end of the clip back inside it', () => {
    expect(clampRange({ startMs: -500, endMs: 9_999 }, 4_000)).toEqual({
      startMs: 0,
      endMs: 4_000,
    });
  });

  it('grows a collapsed or inverted range from the start it names', () => {
    // Dragging one handle onto the other is the ordinary way a user produces this. It is
    // repaired rather than sent on: the Rust side rejects an empty range, and an error for
    // a drag that looked legal is worse than a selection the user can see.
    expect(clampRange({ startMs: 2_000, endMs: 2_000 }, 4_000)).toEqual({
      startMs: 2_000,
      endMs: 2_100,
    });
    expect(clampRange({ startMs: 2_000, endMs: 1_000 }, 4_000)).toEqual({
      startMs: 2_000,
      endMs: 2_100,
    });
  });

  it('slides the range back when it cannot grow forwards', () => {
    // At the very end of the clip there is no room after the start, so the start moves.
    expect(clampRange({ startMs: 3_999, endMs: 3_999 }, 4_000)).toEqual({
      startMs: 3_900,
      endMs: 4_000,
    });
  });

  it('offers the whole clip when the clip is shorter than the minimum selection', () => {
    expect(clampRange({ startMs: 0, endMs: 10 }, 50)).toEqual({ startMs: 0, endMs: 50 });
    expect(clampRange({ startMs: 40, endMs: 50 }, 50)).toEqual({ startMs: 0, endMs: 50 });
  });

  it('is empty for a clip with no duration', () => {
    expect(clampRange({ startMs: 0, endMs: 1_000 }, 0)).toEqual({ startMs: 0, endMs: 0 });
    expect(clampRange({ startMs: 0, endMs: 1_000 }, -1)).toEqual({ startMs: 0, endMs: 0 });
  });

  it('rounds fractional milliseconds, because that is what trim_clip is given', () => {
    expect(clampRange({ startMs: 100.4, endMs: 200.6 }, 4_000)).toEqual({
      startMs: 100,
      endMs: 201,
    });
  });

  it('never produces a range shorter than the minimum', () => {
    for (const range of [
      { startMs: 0, endMs: 0 },
      { startMs: 3_000, endMs: 1_000 },
      { startMs: 999, endMs: 1_000 },
      { startMs: -1, endMs: 0 },
    ]) {
      const clamped = clampRange(range, 4_000);
      expect(clamped.endMs - clamped.startMs).toBeGreaterThanOrEqual(MIN_TRIM_MS);
      expect(clamped.startMs).toBeGreaterThanOrEqual(0);
      expect(clamped.endMs).toBeLessThanOrEqual(4_000);
    }
  });
});

describe('fullRange', () => {
  it('covers the whole clip', () => {
    expect(fullRange(4_000)).toEqual({ startMs: 0, endMs: 4_000 });
  });

  it('is empty for a clip with no duration', () => {
    expect(fullRange(0)).toEqual({ startMs: 0, endMs: 0 });
  });
});

describe('dragHandle', () => {
  it('moves only the handle that was dragged', () => {
    expect(dragHandle({ startMs: 1_000, endMs: 3_000 }, 'start', 2_500, 4_000)).toEqual({
      startMs: 2_500,
      endMs: 3_000,
    });
    expect(dragHandle({ startMs: 1_000, endMs: 3_000 }, 'end', 1_500, 4_000)).toEqual({
      startMs: 1_000,
      endMs: 1_500,
    });
  });

  it('will not let the start handle cross the end handle', () => {
    expect(dragHandle({ startMs: 1_000, endMs: 3_000 }, 'start', 9_000, 4_000)).toEqual({
      startMs: 2_900,
      endMs: 3_000,
    });
  });

  it('will not let the end handle cross the start handle', () => {
    expect(dragHandle({ startMs: 1_000, endMs: 3_000 }, 'end', 0, 4_000)).toEqual({
      startMs: 1_000,
      endMs: 1_100,
    });
  });

  it('holds both handles inside the clip', () => {
    expect(dragHandle({ startMs: 1_000, endMs: 3_000 }, 'start', -9_000, 4_000)).toEqual({
      startMs: 0,
      endMs: 3_000,
    });
    expect(dragHandle({ startMs: 1_000, endMs: 3_000 }, 'end', 9_000, 4_000)).toEqual({
      startMs: 1_000,
      endMs: 4_000,
    });
  });

  it('does nothing to a clip with no duration', () => {
    expect(dragHandle({ startMs: 0, endMs: 1_000 }, 'end', 500, 0)).toEqual({
      startMs: 0,
      endMs: 0,
    });
  });
});

describe('nudgeHandle', () => {
  it('moves a handle by one step for the keyboard', () => {
    expect(nudgeHandle({ startMs: 1_000, endMs: 3_000 }, 'start', KEYBOARD_STEP_MS, 4_000)).toEqual({
      startMs: 1_100,
      endMs: 3_000,
    });
    expect(nudgeHandle({ startMs: 1_000, endMs: 3_000 }, 'end', -KEYBOARD_STEP_MS, 4_000)).toEqual({
      startMs: 1_000,
      endMs: 2_900,
    });
  });

  it('stops at the ends of the clip, exactly like a drag does', () => {
    expect(nudgeHandle({ startMs: 0, endMs: 3_000 }, 'start', -1_000, 4_000)).toEqual({
      startMs: 0,
      endMs: 3_000,
    });
    expect(nudgeHandle({ startMs: 1_000, endMs: 4_000 }, 'end', 1_000, 4_000)).toEqual({
      startMs: 1_000,
      endMs: 4_000,
    });
  });

  it('cannot collapse the range by nudging one handle into the other', () => {
    expect(nudgeHandle({ startMs: 2_900, endMs: 3_000 }, 'start', 1_000, 4_000)).toEqual({
      startMs: 2_900,
      endMs: 3_000,
    });
  });
});

describe('validateRange', () => {
  it('accepts a range inside the clip', () => {
    expect(validateRange({ startMs: 1_000, endMs: 3_000 }, 4_000)).toEqual({
      ok: true,
      range: { startMs: 1_000, endMs: 3_000 },
    });
  });

  it('accepts a range that ends exactly at the end of the clip', () => {
    // The end bound is inclusive, which is what a scrubber dragged to the right edge asks
    // for — and it is the same bound trim_clip accepts.
    expect(validateRange({ startMs: 0, endMs: 4_000 }, 4_000)).toEqual({
      ok: true,
      range: { startMs: 0, endMs: 4_000 },
    });
  });

  it('accepts a range exactly at the minimum length', () => {
    const result = validateRange({ startMs: 0, endMs: MIN_TRIM_MS }, 4_000);
    expect(result.ok).toBe(true);
  });

  it('refuses an empty or inverted range, and says so', () => {
    const collapsed = validateRange({ startMs: 2_000, endMs: 2_000 }, 4_000);
    expect(collapsed.ok).toBe(false);
    if (!collapsed.ok) expect(collapsed.reason).toContain('empty');

    const inverted = validateRange({ startMs: 3_000, endMs: 1_000 }, 4_000);
    expect(inverted.ok).toBe(false);
    if (!inverted.ok) expect(inverted.reason).toContain('empty');
  });

  it('refuses a range that reaches past the end of the clip, naming the duration', () => {
    const result = validateRange({ startMs: 0, endMs: 4_001 }, 4_000);
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.reason).toContain('0:04.001');
      expect(result.reason).toContain('0:04.0');
    }
  });

  it('refuses a range shorter than the minimum', () => {
    const result = validateRange({ startMs: 0, endMs: MIN_TRIM_MS - 1 }, 4_000);
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.reason).toContain('shortest selection');
  });

  it('refuses everything about a clip with no duration', () => {
    const result = validateRange({ startMs: 0, endMs: 0 }, 0);
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.reason).toContain('no duration');
  });
});

describe('timeline geometry', () => {
  it('places a position as a percentage of the clip', () => {
    expect(msToPercent(2_000, 4_000)).toBe(50);
    expect(msToPercent(0, 4_000)).toBe(0);
    expect(msToPercent(4_000, 4_000)).toBe(100);
  });

  it('holds the playhead inside the track even if the media time is not', () => {
    expect(msToPercent(-100, 4_000)).toBe(0);
    expect(msToPercent(9_000, 4_000)).toBe(100);
    expect(msToPercent(1_000, 0)).toBe(0);
  });

  it('turns a fraction of the track back into a position', () => {
    expect(fractionToMs(0.5, 4_000)).toBe(2_000);
    expect(fractionToMs(0.333, 4_000)).toBe(1_332);
    expect(fractionToMs(-1, 4_000)).toBe(0);
    expect(fractionToMs(2, 4_000)).toBe(4_000);
    expect(fractionToMs(0.5, 0)).toBe(0);
  });

  it('describes the selection band as a left edge and a width', () => {
    expect(rangePercents({ startMs: 1_000, endMs: 2_000 }, 4_000)).toEqual({
      left: 25,
      width: 25,
    });
    expect(rangePercents({ startMs: 0, endMs: 4_000 }, 4_000)).toEqual({ left: 0, width: 100 });
    expect(rangePercents({ startMs: 0, endMs: 1_000 }, 0)).toEqual({ left: 0, width: 0 });
  });

  it('rounds the readout to the milliseconds the command will be given', () => {
    expect(rangeLabel({ startMs: 2_000, endMs: 5_000 })).toBe('0:02.000 → 0:05.000');
    expect(rangeLengthLabel({ startMs: 2_000, endMs: 5_000 })).toBe('0:03.0');
    expect(rangeLengthLabel({ startMs: 5_000, endMs: 2_000 })).toBe('0:00.0');
  });
});
