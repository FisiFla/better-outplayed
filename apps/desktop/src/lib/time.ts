/**
 * Formatting. Pure functions over numbers — no DOM, no state, no locale services — so the
 * display rules are pinned by tests rather than eyeballed in a window this project cannot
 * open (see the README's "what is not verified").
 */

const MS_PER_SECOND = 1_000;
const MS_PER_MINUTE = 60 * MS_PER_SECOND;
const MS_PER_HOUR = 60 * MS_PER_MINUTE;

/**
 * Clamp a number into a range, mapping `NaN` to the low end.
 *
 * `NaN` is the only special case: it is what arithmetic on a missing value produces, and it
 * has no position in any range, so the low bound is as good an answer as any and a far
 * better one than `NaN` propagating into a readout. Infinities clamp normally —
 * `clamp(Infinity, 0, 3)` is `3` — because `Math.min`/`Math.max` already order them
 * correctly, and pretending an infinite value is the *low* end would be a lie in the
 * opposite direction.
 */
export function clamp(value: number, low: number, high: number): number {
  if (Number.isNaN(value)) return low;
  return Math.min(Math.max(value, low), high);
}

function pad(value: number, width: number): string {
  return String(value).padStart(width, '0');
}

/**
 * `M:SS.d`, or `H:MM:SS.d` past an hour — the length of a clip, or of a selected range.
 *
 * One decimal place because clips are seconds to minutes long and a tenth is the
 * granularity a person reads; milliseconds belong in [`formatPrecise`].
 */
export function formatDuration(ms: number): string {
  // A length that is not a finite number is not a length. Rendering it as "0:00.0" would
  // claim the clip is empty, and rendering `Infinity` would print a number with more digits
  // than the pane is wide; a dash says what is true, which is that there is no value.
  if (!Number.isFinite(ms)) return '—';
  const total = Math.max(0, Math.round(ms));
  // Round to tenths first, so 59_960ms renders as "1:00.0" and not "0:60.0".
  const tenths = Math.round(total / 100);
  const seconds = Math.floor(tenths / 10);
  const hours = Math.floor(seconds / 3_600);
  const minutes = Math.floor((seconds % 3_600) / 60);
  const rest = seconds % 60;
  const fraction = tenths % 10;

  const mmss = `${minutes}:${pad(rest, 2)}.${fraction}`;
  return hours > 0 ? `${hours}:${pad(minutes, 2)}:${pad(rest, 2)}.${fraction}` : mmss;
}

/**
 * `M:SS.mmm` — a position on the timeline, for the scrubber's readouts.
 *
 * Exact milliseconds, because this is the number a user checks against the handles they
 * just dragged, and it is the number that is sent to `trim_clip`.
 */
export function formatPrecise(ms: number): string {
  if (!Number.isFinite(ms)) return '—';
  const total = Math.max(0, Math.round(ms));
  const hours = Math.floor(total / MS_PER_HOUR);
  const minutes = Math.floor((total % MS_PER_HOUR) / MS_PER_MINUTE);
  const seconds = Math.floor((total % MS_PER_MINUTE) / MS_PER_SECOND);
  const millis = total % MS_PER_SECOND;

  const mmss = `${minutes}:${pad(seconds, 2)}.${pad(millis, 3)}`;
  return hours > 0 ? `${hours}:${pad(minutes, 2)}:${pad(seconds, 2)}.${pad(millis, 3)}` : mmss;
}

const BYTE_UNITS = ['B', 'KiB', 'MiB', 'GiB', 'TiB'] as const;

/**
 * A byte count at a readable scale, as the storage panel and the clip list show it.
 *
 * 1024-based (KiB/MiB/GiB) because that is what `storage.max_total_bytes` is written in —
 * `53687091200` is documented in the spec as "50 GiB" — so the panel's arithmetic and the
 * config file's numbers agree.
 */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes)) return '—';
  const value = Math.max(0, bytes);
  if (value < 1024) return `${Math.round(value)} B`;

  let scale = value;
  let unit = 0;
  while (scale >= 1024 && unit < BYTE_UNITS.length - 1) {
    scale /= 1024;
    unit += 1;
  }
  // One decimal below 10, none above it: "9.8 MiB", "812 MiB", "1.4 GiB".
  const rendered = scale < 10 ? scale.toFixed(1) : Math.round(scale).toString();
  return `${rendered} ${BYTE_UNITS[unit]}`;
}

/**
 * A wall-clock instant in the machine's local time, as `YYYY-MM-DD HH:MM`.
 *
 * Local, not UTC: this is the "when did I record this" column, and a user comparing it
 * against their own evening should not have to do timezone arithmetic. The tests assert
 * the shape rather than a literal, because the literal depends on the host's zone.
 */
export function formatDateTime(epochMs: number): string {
  if (!Number.isFinite(epochMs)) return '—';
  const date = new Date(epochMs);
  if (Number.isNaN(date.getTime())) return '—';

  const day = `${date.getFullYear()}-${pad(date.getMonth() + 1, 2)}-${pad(date.getDate(), 2)}`;
  const time = `${pad(date.getHours(), 2)}:${pad(date.getMinutes(), 2)}`;
  return `${day} ${time}`;
}

/** A percentage of a whole, for the storage bar. `0` when the whole is not a whole. */
export function percentOf(part: number, whole: number): number {
  if (!Number.isFinite(part) || !Number.isFinite(whole) || whole <= 0) return 0;
  return clamp((part / whole) * 100, 0, 100);
}
