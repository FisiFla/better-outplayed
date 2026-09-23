import { describe, expect, it } from 'vitest';
import {
  clamp,
  formatBytes,
  formatDateTime,
  formatDuration,
  formatPrecise,
  percentOf,
} from './time';

describe('formatDuration', () => {
  it('renders a clip length as M:SS.d', () => {
    expect(formatDuration(0)).toBe('0:00.0');
    expect(formatDuration(12_345)).toBe('0:12.3');
    expect(formatDuration(59_000)).toBe('0:59.0');
  });

  it('carries the tenths into the seconds instead of printing 0:60.0', () => {
    // 59.96s rounds to 60.0s, which is 1:00.0 — the rounding has to happen before the
    // split, or the readout shows a seconds field of 60.
    expect(formatDuration(59_960)).toBe('1:00.0');
    expect(formatDuration(999)).toBe('0:01.0');
  });

  it('adds hours only when there are any', () => {
    expect(formatDuration(60_000)).toBe('1:00.0');
    expect(formatDuration(3_599_000)).toBe('59:59.0');
    expect(formatDuration(3_600_000)).toBe('1:00:00.0');
    expect(formatDuration(3_723_400)).toBe('1:02:03.4');
  });

  it('cannot render a negative length as anything but zero', () => {
    expect(formatDuration(-5_000)).toBe('0:00.0');
  });

  it('renders a length that is not a finite number as a dash, not as zero', () => {
    // "0:00.0" would claim the clip is empty, which is a different statement from "this
    // number is not a duration at all".
    expect(formatDuration(Number.NaN)).toBe('—');
    expect(formatDuration(Number.POSITIVE_INFINITY)).toBe('—');
  });
});

describe('formatPrecise', () => {
  it('renders a timeline position to the millisecond', () => {
    expect(formatPrecise(0)).toBe('0:00.000');
    expect(formatPrecise(2_100)).toBe('0:02.100');
    expect(formatPrecise(65_432)).toBe('1:05.432');
  });

  it('pads the millisecond field, so two readouts line up', () => {
    expect(formatPrecise(1_005)).toBe('0:01.005');
    expect(formatPrecise(1_050)).toBe('0:01.050');
  });

  it('adds hours only when there are any', () => {
    expect(formatPrecise(3_600_500)).toBe('1:00:00.500');
  });

  it('rounds a fractional millisecond rather than printing one', () => {
    expect(formatPrecise(1_500.6)).toBe('0:01.501');
  });

  it('renders a position that is not a finite number as a dash', () => {
    expect(formatPrecise(Number.NaN)).toBe('—');
    expect(formatPrecise(Number.POSITIVE_INFINITY)).toBe('—');
  });
});

describe('formatBytes', () => {
  it('keeps bytes below a kibibyte as bytes', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(1023)).toBe('1023 B');
  });

  it('scales in 1024s, matching how the cap is written in the config', () => {
    expect(formatBytes(1024)).toBe('1.0 KiB');
    expect(formatBytes(812 * 1024)).toBe('812 KiB');
    expect(formatBytes(1503238554)).toBe('1.4 GiB');
  });

  it('renders the example config cap as the spec describes it', () => {
    // config.example.toml's `max_total_bytes = 53687091200` is documented as "50 GiB", so
    // the panel and the config file have to agree on what that number is.
    expect(formatBytes(53_687_091_200)).toBe('50 GiB');
    expect(formatBytes(5_368_709_120)).toBe('5.0 GiB');
  });

  it('never renders a negative size as a negative number', () => {
    expect(formatBytes(-1)).toBe('0 B');
  });

  it('renders a size that is not a finite number as a dash', () => {
    expect(formatBytes(Number.NaN)).toBe('—');
    expect(formatBytes(Number.POSITIVE_INFINITY)).toBe('—');
  });
});

describe('formatDateTime', () => {
  it('renders a wall-clock instant as YYYY-MM-DD HH:MM', () => {
    // The literal depends on the host's timezone, so the shape is what is pinned; the
    // point of this formatter is that a user reads their own local evening off it.
    expect(formatDateTime(1_756_000_000_000)).toMatch(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$/);
  });

  it('shows a dash rather than "Invalid Date" for a missing instant', () => {
    expect(formatDateTime(Number.NaN)).toBe('—');
    expect(formatDateTime(Number.POSITIVE_INFINITY)).toBe('—');
  });
});

describe('clamp', () => {
  it('bounds a value', () => {
    expect(clamp(5, 0, 3)).toBe(3);
    expect(clamp(-5, 0, 3)).toBe(0);
    expect(clamp(2, 0, 3)).toBe(2);
  });

  it('maps NaN to the low bound, because NaN has no position in a range', () => {
    expect(clamp(Number.NaN, 0, 3)).toBe(0);
  });

  it('clamps infinities to the bound they are past, not to the low one', () => {
    expect(clamp(Number.POSITIVE_INFINITY, 0, 3)).toBe(3);
    expect(clamp(Number.NEGATIVE_INFINITY, 0, 3)).toBe(0);
  });
});

describe('percentOf', () => {
  it('is a percentage of the whole', () => {
    expect(percentOf(700, 1_000)).toBe(70);
    expect(percentOf(1_000, 1_000)).toBe(100);
  });

  it('does not divide by a cap of zero', () => {
    expect(percentOf(700, 0)).toBe(0);
    expect(percentOf(700, -1)).toBe(0);
  });

  it('stays inside the bar even when the whole is exceeded', () => {
    expect(percentOf(2_000, 1_000)).toBe(100);
    expect(percentOf(-5, 1_000)).toBe(0);
  });
});
