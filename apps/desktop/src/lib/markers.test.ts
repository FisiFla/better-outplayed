/**
 * The marker arithmetic, tested without a DOM.
 *
 * These are the properties a screenshot cannot show: that an unknown tag is *visible* rather
 * than dropped, that a marker outside the clip's window is not lost, and that a click seeks
 * somewhere a person would recognise.
 */

import { describe, expect, it } from 'vitest';

import {
  CLICK_SLOP_PX,
  MARKER_LEAD_MS,
  NEUTRAL_MARKER_COLOR,
  isClick,
  isKnownKind,
  markerColor,
  markerLabel,
  markerPercent,
  placeMarkers,
  seekTargetMs,
} from './markers';
import type { SessionEvent } from './types';

function event(offset_ms: number, kind: string): SessionEvent {
  return { id: offset_ms, kind, offset_ms, payload: null, clip_id: null };
}

describe('the marker vocabulary', () => {
  it('gives a known tag its own colour', () => {
    expect(markerColor('kill')).not.toBe(NEUTRAL_MARKER_COLOR);
    expect(markerColor('bookmark')).toBe('var(--accent)');
    expect(isKnownKind('kill')).toBe(true);
  });

  it('shows an unknown tag neutrally rather than hiding it', () => {
    // The whole point: `localplay-store` passes `events.kind` through verbatim and knows no
    // vocabulary. A tag this build has never heard of is a marker that exists, and an
    // unrecognised integration must not silently lose its timeline.
    expect(markerColor('bomb_planted_v2')).toBe(NEUTRAL_MARKER_COLOR);
    expect(isKnownKind('bomb_planted_v2')).toBe(false);
    // Named as itself, not as "unknown": that is what lets someone ask what it is.
    expect(markerLabel('bomb_planted_v2')).toBe('bomb planted v2');
    expect(markerLabel('kill')).toBe('kill');
  });
});

describe('markerPercent', () => {
  it('places a marker proportionally along the track', () => {
    expect(markerPercent(0, 10_000)).toBe(0);
    expect(markerPercent(2_500, 10_000)).toBe(25);
    expect(markerPercent(10_000, 10_000)).toBe(100);
  });

  it('clamps rather than dropping a marker outside the clip window', () => {
    // A clip cut out of a session carries the session's offsets, so a marker can legitimately
    // sit past the clip's own end. Visible at the edge beats silently absent.
    expect(markerPercent(30_000, 10_000)).toBe(100);
    expect(markerPercent(-5_000, 10_000)).toBe(0);
  });

  it('collapses to 0 when there is no axis, rather than poisoning the track with NaN', () => {
    expect(markerPercent(1_000, 0)).toBe(0);
    expect(markerPercent(1_000, -1)).toBe(0);
    expect(markerPercent(Number.NaN, 10_000)).toBe(0);
  });
});

describe('seekTargetMs', () => {
  it('seeks the lead-in before the marker, so the mark is not the first frame', () => {
    expect(seekTargetMs(10_000)).toBe(10_000 - MARKER_LEAD_MS);
  });

  it('never seeks before the start of the clip', () => {
    // A negative `currentTime` is ignored by the media element, which would make an early
    // marker look like a dead button.
    expect(seekTargetMs(1_000)).toBe(0);
    expect(seekTargetMs(0)).toBe(0);
    expect(seekTargetMs(-500)).toBe(0);
  });

  it('takes the lead as a parameter so a caller can seek exactly when it wants to', () => {
    expect(seekTargetMs(10_000, 0)).toBe(10_000);
    expect(seekTargetMs(10_000, 500)).toBe(9_500);
  });
});

describe('isClick', () => {
  it('treats a still pointer as a click and a moved one as a drag', () => {
    const down = { x: 100, y: 20 };
    expect(isClick(down, { x: 100, y: 20 })).toBe(true);
    expect(isClick(down, { x: 100 + CLICK_SLOP_PX, y: 20 })).toBe(true);
    expect(isClick(down, { x: 100 + CLICK_SLOP_PX + 1, y: 20 })).toBe(false);
    expect(isClick(down, { x: 100, y: 20 + CLICK_SLOP_PX + 1 })).toBe(false);
  });
});

describe('placeMarkers', () => {
  it('computes position, colour and label for every marker', () => {
    const placed = placeMarkers([event(5_000, 'kill'), event(2_500, 'bookmark')], 10_000);
    expect(placed.map((m) => [m.percent, m.color, m.label])).toEqual([
      [50, markerColor('kill'), 'kill'],
      [25, 'var(--accent)', 'bookmark'],
    ]);
  });

  it('preserves the store order rather than sorting', () => {
    // The store returns markers in media-time order and breaks ties by row id. Re-sorting
    // here would be a second opinion about an order that is already defined.
    const shuffled = [event(9_000, 'kill'), event(1_000, 'death')];
    expect(placeMarkers(shuffled, 10_000).map((m) => m.event.offset_ms)).toEqual([9_000, 1_000]);
  });

  it('has nothing to draw for a session nobody tagged', () => {
    expect(placeMarkers([], 10_000)).toEqual([]);
  });
});
