import { describe, expect, it } from 'vitest';
import {
  LOW_DISK_BYTES,
  describeDisk,
  describeEncoder,
  describeEventSources,
  describeGsiInstall,
  describeRecorderState,
  diskIsLow,
  needsAttention,
} from './health';
import type { Health } from './types';

function health(overrides: Partial<Health> = {}): Health {
  return {
    recorder_state: 'idle',
    recorder_error: null,
    encoder: 'h264_nvenc',
    ffmpeg_available: true,
    disk: { path: 'C:\\clips', free_bytes: 100 * 1024 ** 3, total_bytes: 500 * 1024 ** 3 },
    event_sources: [],
    recent_errors: [],
    log_path: 'C:\\logs\\replayed.log',
    ...overrides,
  };
}

describe('the health wording', () => {
  it('names every recorder state, and carries the error for a failure', () => {
    expect(describeRecorderState('recording', null)).toBe('Recording');
    expect(describeRecorderState('waiting', null)).toBe('Armed, waiting for a game');
    expect(describeRecorderState('idle', null)).toBe('Not recording');
    expect(describeRecorderState('failed', 'encoder died')).toBe('Stopped by an error: encoder died');
  });

  it('says what the encoder is, or why there is none', () => {
    expect(describeEncoder(health())).toBe('h264_nvenc');
    expect(describeEncoder(health({ encoder: null }))).toContain('None yet');
    expect(describeEncoder(health({ ffmpeg_available: false }))).toContain('ffmpeg was not found');
  });

  it('flags low disk below the threshold only', () => {
    const at = (free: number) => ({ path: 'x', free_bytes: free, total_bytes: LOW_DISK_BYTES * 10 });
    expect(diskIsLow(at(LOW_DISK_BYTES - 1))).toBe(true);
    expect(diskIsLow(at(LOW_DISK_BYTES))).toBe(false);
    expect(diskIsLow(null)).toBe(false);
    expect(describeDisk(null)).toBe('Unknown');
    expect(describeDisk(at(LOW_DISK_BYTES))).toContain('free of');
  });

  it('lists event sources with their state and detail', () => {
    const lines = describeEventSources(
      health({
        event_sources: [{ source: 'CS2 / Dota 2', state: 'failed', detail: 'address in use' }],
      }),
    );
    expect(lines).toEqual(['CS2 / Dota 2: failed, address in use']);
  });

  it('needs attention for a failure, missing ffmpeg, low disk or a failed source, and not otherwise', () => {
    expect(needsAttention(health())).toBe(false);
    expect(needsAttention(health({ recorder_state: 'failed' }))).toBe(true);
    expect(needsAttention(health({ ffmpeg_available: false }))).toBe(true);
    expect(needsAttention(health({ disk: { path: 'x', free_bytes: 1, total_bytes: 2 } }))).toBe(true);
    expect(
      needsAttention(health({ event_sources: [{ source: 'a', state: 'failed', detail: 'b' }] })),
    ).toBe(true);
    expect(
      needsAttention(health({ event_sources: [{ source: 'a', state: 'disabled', detail: 'b' }] })),
    ).toBe(false);
  });

  it('distinguishes a fresh install from an update, and tells the user to restart the game', () => {
    const fresh = describeGsiInstall({ game: 'cs2', path: 'P', replaced: false }, 'Counter-Strike 2');
    const again = describeGsiInstall({ game: 'cs2', path: 'P', replaced: true }, 'Counter-Strike 2');
    expect(fresh.startsWith('Installed')).toBe(true);
    expect(again.startsWith('Updated')).toBe(true);
    expect(fresh).toContain('Restart the game');
  });
});
