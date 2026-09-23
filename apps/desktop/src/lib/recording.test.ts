/**
 * The recording readout: the rules that decide what a person sees.
 *
 * Same reasoning as `time.test.ts` and `clips.test.ts`: this window cannot be opened by the
 * test suite, so the display rules are pinned here. The values below are the shapes
 * `localplay-recorder` really publishes — including the two that are easy to render
 * dishonestly: a rate that has not been measured yet (0.0 on a healthy recorder) and a clip
 * that was written but not indexed (no row at all).
 */

import { describe, expect, it } from 'vitest';
import {
  describeFrames,
  describeRecordedClip,
  formatDrift,
  formatRate,
  recordingLabel,
  recordingState,
} from './recording';
import type { RecordedClip, RecordingStatus } from './types';

/** A running recorder's status, with the fields each test cares about overridden. */
function status(overrides: Partial<RecordingStatus> = {}): RecordingStatus {
  return {
    running: true,
    frames: 480,
    segments: 12,
    bytes: 41_943_040,
    span_ms: 12_000,
    dropped: 0,
    dropped_audio: 0,
    skipped: 320,
    fps: 29.83,
    configured_fps: 30,
    drift_ms: 1_258,
    clips: 1,
    error: null,
    ...overrides,
  };
}

describe('which state the panel is in', () => {
  it('is idle before the first status arrives', () => {
    expect(recordingState(null)).toBe('idle');
    expect(recordingLabel(null)).toBe('not recording');
  });

  it('is recording while the engine is running', () => {
    expect(recordingState(status())).toBe('recording');
    expect(recordingLabel(status())).toBe('REC');
  });

  it('is idle again after a clean stop', () => {
    expect(recordingState(status({ running: false }))).toBe('idle');
    expect(recordingLabel(status({ running: false }))).toBe('not recording');
  });

  it('reports a failure as its own state, whatever the running flag says', () => {
    // The engine sets `error` when the loop stopped for a failure. Showing that under a
    // "not recording" label would hide the one thing the user needs to read.
    const failed = status({ running: false, error: 'scratch cap violated: …' });
    expect(recordingState(failed)).toBe('failed');
    expect(recordingLabel(failed)).toBe('stopped');
  });
});

describe('the achieved rate', () => {
  it('is a dash when nothing is recording', () => {
    expect(formatRate(null)).toBe('—');
    expect(formatRate(status({ running: false }))).toBe('—');
  });

  it('says it is still measuring rather than printing 0.0fps', () => {
    // `RateMeter` reports 0.0 until a whole one-second window has closed, deliberately: a
    // rate over the first 200ms is noise. On a healthy recorder that 0.0 is not a failure,
    // and "0.0 / 30 fps" beside a running capture would read as one.
    expect(formatRate(status({ fps: 0 }))).toBe('measuring…');
  });

  it('shows the achieved rate against the configured one', () => {
    expect(formatRate(status())).toBe('29.8 / 30 fps');
    expect(formatRate(status({ fps: 60, configured_fps: 60 }))).toBe('60.0 / 60 fps');
    expect(formatRate(status({ fps: 23.94, configured_fps: 30 }))).toBe('23.9 / 30 fps');
  });
});

describe('media drift', () => {
  it('is a dash when nothing is recording', () => {
    expect(formatDrift(null)).toBe('—');
    expect(formatDrift(status({ running: false }))).toBe('—');
  });

  it('says which way the media timeline is off', () => {
    // Positive is wall minus media: the measured 0.81x case, where 1000ms of media costs
    // ~1230ms of real time.
    expect(formatDrift(status({ drift_ms: 1_258 }))).toBe('1258ms behind real time');
    // The opposite can happen too (the encoder drained its pipe faster than the clock).
    expect(formatDrift(status({ drift_ms: -80 }))).toBe('80ms ahead of real time');
    expect(formatDrift(status({ drift_ms: 0 }))).toBe('0ms behind real time');
  });
});

describe('the frame accounting', () => {
  it('says nothing when there is nothing to report', () => {
    expect(describeFrames(status({ skipped: 0 }))).toBeNull();
    expect(describeFrames(null)).toBeNull();
  });

  it('names the encoder drop as the failure it is', () => {
    const text = describeFrames(status({ dropped: 6 }));
    expect(text).toContain('6 frames dropped by the encoder');
    expect(text).toContain('not keeping up with 30fps');
  });

  it('reports a skipped frame as an optimisation, not a loss', () => {
    // `skipped` is the pacer discarding surplus frames *without* the GPU readback — the
    // signature of the optimisation working. It is reported without alarm.
    expect(describeFrames(status({ skipped: 320, dropped: 0 }))).toBe(
      '320 frames skipped without a GPU readback.',
    );
  });

  it('mentions dropped audio separately, because it is a different failure', () => {
    const text = describeFrames(status({ skipped: 0, dropped_audio: 4 }));
    expect(text).toBe('4 audio blocks dropped.');
  });
});

describe('the notice for a clip that was just taken', () => {
  const clip: RecordedClip = {
    id: 3,
    path: 'C:\\Users\\player\\AppData\\Local\\localplay\\clips\\clip-1790184357.mp4',
    duration_ms: 12_021,
    size_bytes: 26_624,
    codec: 'h264_nvenc',
    started_at_ms: 4_000,
  };

  it('names the file, its length, its size and the row', () => {
    expect(describeRecordedClip(clip)).toBe(
      'Saved clip-1790184357.mp4 (0:12.0, 26 KiB, h264_nvenc) as clip #3.',
    );
  });

  it('says when the file was written but not indexed', () => {
    // The one failure the trigger path can leave behind: the clip exists and the row does
    // not. Calling it "saved" would be a lie the user only discovers when the list is
    // missing it and the storage policy never manages it.
    const text = describeRecordedClip({ ...clip, id: null });
    expect(text).toContain('Wrote clip-1790184357.mp4');
    expect(text).toContain('could not be indexed');
    expect(text).toContain('will not appear in the list');
    expect(text).not.toContain('Saved');
  });
});
