import { describe, expect, it } from "vitest";
import {
  clamp,
  formatBytes,
  formatDateTime,
  formatDuration,
  formatPrecise,
  percentOf,
} from "./time";

const NOT_FINITE = [Number.NaN, Number.POSITIVE_INFINITY];

describe("formatDuration", () => {
  it.each([
    [0, "0:00.0"],
    [12_345, "0:12.3"],
    [59_000, "0:59.0"],
    // 59.96s rounds to 60.0s: the rounding has to happen before the split, or the readout
    // shows a seconds field of 60.
    [59_960, "1:00.0"],
    [999, "0:01.0"],
    [59 * 60_000 + 59_000, "59:59.0"],
    [3_600_000, "1:00:00.0"],
    [3_723_400, "1:02:03.4"],
    [-5_000, "0:00.0"],
  ])("renders %d ms as %s", (ms, text) => {
    expect(formatDuration(ms)).toBe(text);
  });

  // "0:00.0" would claim the clip is empty, which is a different statement from "this
  // number is not a duration at all".
  it.each(NOT_FINITE)("renders %d as a dash, not as zero", (ms) => {
    expect(formatDuration(ms)).toBe("—");
  });
});

describe("formatPrecise", () => {
  it.each([
    [0, "0:00.000"],
    [65_432, "1:05.432"],
    [1_005, "0:01.005"], // the millisecond field is padded
    [3_600_500, "1:00:00.500"],
    [1_500.6, "0:01.501"], // a fractional millisecond is rounded
  ])("renders %d ms as %s", (ms, text) => {
    expect(formatPrecise(ms)).toBe(text);
  });

  it.each(NOT_FINITE)("renders %d as a dash", (ms) => {
    expect(formatPrecise(ms)).toBe("—");
  });
});

describe("formatBytes", () => {
  it.each([
    [0, "0 B"],
    [1023, "1023 B"],
    [1024, "1.0 KiB"],
    [812 * 1024, "812 KiB"],
    [1503238554, "1.4 GiB"],
    // config.example.toml's `max_total_bytes = 53687091200` is documented as "50 GiB".
    [53_687_091_200, "50 GiB"],
    [-1, "0 B"],
  ])("renders %d bytes as %s", (bytes, text) => {
    expect(formatBytes(bytes)).toBe(text);
  });

  it.each(NOT_FINITE)("renders %d as a dash", (bytes) => {
    expect(formatBytes(bytes)).toBe("—");
  });
});

describe("formatDateTime", () => {
  it("renders a wall-clock instant as YYYY-MM-DD HH:MM", () => {
    // The literal depends on the host's timezone, so the shape is what is pinned.
    expect(formatDateTime(1_756_000_000_000)).toMatch(
      /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$/,
    );
  });

  it.each(NOT_FINITE)(
    'shows a dash rather than "Invalid Date" for %d',
    (ms) => {
      expect(formatDateTime(ms)).toBe("—");
    },
  );
});

describe("clamp", () => {
  it.each([
    [5, 3],
    [-5, 0],
    [2, 2],
    [Number.NaN, 0], // NaN has no position in a range
    [Number.POSITIVE_INFINITY, 3],
    [Number.NEGATIVE_INFINITY, 0],
  ])("clamp(%d, 0, 3) is %d", (value, expected) => {
    expect(clamp(value, 0, 3)).toBe(expected);
  });
});

describe("percentOf", () => {
  it.each([
    [700, 1_000, 70],
    [1_000, 1_000, 100],
    [700, 0, 0], // no division by a cap of zero
    [700, -1, 0],
    [2_000, 1_000, 100], // stays inside the bar when the whole is exceeded
    [-5, 1_000, 0],
  ])("percentOf(%d, %d) is %d", (part, whole, expected) => {
    expect(percentOf(part, whole)).toBe(expected);
  });
});
