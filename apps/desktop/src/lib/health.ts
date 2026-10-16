/**
 * The words the health page and the integration installer show.
 *
 * Pure, like the other `lib/*.ts` files: the window cannot be opened in the suite, so the
 * wording and the thresholds are pinned here instead of by a screenshot.
 */

import { formatBytes } from './time';
import type { DiskHealth, GsiInstallOutcome, Health, RecorderState } from './types';

/** Below this much free space the page warns: a clip and its scratch can be a few GB. */
export const LOW_DISK_BYTES = 5 * 1024 ** 3;

export function describeRecorderState(state: RecorderState, error: string | null): string {
  switch (state) {
    case 'recording':
      return 'Recording';
    case 'waiting':
      return 'Armed, waiting for a game';
    case 'failed':
      return error === null ? 'Stopped by an error' : `Stopped by an error: ${error}`;
    default:
      return 'Not recording';
  }
}

export function describeDisk(disk: DiskHealth | null): string {
  if (disk === null) return 'Unknown';
  return `${formatBytes(disk.free_bytes)} free of ${formatBytes(disk.total_bytes)}`;
}

export function diskIsLow(disk: DiskHealth | null): boolean {
  return disk !== null && disk.free_bytes < LOW_DISK_BYTES;
}

export function describeEncoder(health: Health): string {
  if (!health.ffmpeg_available) return 'ffmpeg was not found, so nothing can be encoded';
  return health.encoder ?? 'None yet; chosen when a recording starts';
}

export function describeEventSources(health: Health): string[] {
  return health.event_sources.map((s) => `${s.source}: ${s.state}, ${s.detail}`);
}

/** Whether anything on the page deserves attention (drives the summary's marker). */
export function needsAttention(health: Health): boolean {
  return (
    health.recorder_state === 'failed' ||
    !health.ffmpeg_available ||
    diskIsLow(health.disk) ||
    health.event_sources.some((s) => s.state === 'failed')
  );
}

export function describeGsiInstall(outcome: GsiInstallOutcome, label: string): string {
  const verb = outcome.replaced ? 'Updated' : 'Installed';
  return `${verb} the ${label} integration at ${outcome.path}. Restart the game for it to take effect.`;
}
