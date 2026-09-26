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
  describeAutoRecord,
  describeConfig,
  describeFrames,
  describeHotkey,
  describeRecordedClip,
  formatDrift,
  formatRate,
  recordingLabel,
  recordingState,
} from './recording';
import type { AppStatus, RecordedClip, RecordingStatus } from './types';

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
    effective_fps: 30,
    drift_ms: 1_258,
    clips: 1,
    error: null,
    watching_games: false,
    matched_game: null,
    recorder_mode: 'buffer',
    mic_enabled: false,
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

  it('shows the achieved rate against the rate the pipeline is running at', () => {
    expect(formatRate(status())).toBe('29.8 / 30 fps');
    expect(formatRate(status({ fps: 60, configured_fps: 60, effective_fps: 60 }))).toBe(
      '60.0 / 60 fps',
    );
    // A shortfall *within* the rate the pipeline declared — the machine is not even holding
    // what it measured it could, which is what the engine's drop warning is about.
    expect(formatRate(status({ fps: 23.94 }))).toBe('23.9 / 30 fps');
  });

  it('names the configured rate when the engine had to adapt below it', () => {
    // `encode.adapt_fps`: the probe measured 24fps at 4K against a configured 30, so the
    // pipeline declares and paces to 24 (spec §10.1). Showing "24.0 / 30 fps" alone would
    // read as a machine that cannot keep up with itself; the suffix is what makes it the
    // decision it is. The engine logs the same pair at startup.
    expect(formatRate(status({ fps: 23.9, configured_fps: 30, effective_fps: 24 }))).toBe(
      '23.9 / 24 fps (30 configured)',
    );
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
    expect(text).toContain('not keeping up with the 30fps it declared');
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

describe('the hotkey line', () => {
  it('says nothing before the shell has answered', () => {
    expect(describeHotkey(null)).toBeNull();
  });

  it('names the chord and where it works when a listener is installed', () => {
    const line = describeHotkey({ chord: 'Ctrl+F8', installed: true, error: null });

    expect(line).not.toBeNull();
    expect(line!.problem).toBe(false);
    expect(line!.text).toContain('Ctrl+F8');
    expect(line!.text).toContain('hidden');
  });

  it('is an alert — with the reason — when nothing is listening', () => {
    // The failure this whole feature exists to make visible: the chord is held by another
    // application (or another localplay), so pressing it does nothing. The line has to name
    // the chord *and* the reason, and it has to look like a problem.
    const reason =
      'the chord Ctrl+F8 could not be registered (RegisterHotKey failed: the hot key is ' +
      'already registered)';
    const line = describeHotkey({ chord: 'Ctrl+F8', installed: false, error: reason });

    expect(line!.problem).toBe(true);
    expect(line!.text).toContain('Ctrl+F8');
    expect(line!.text).toContain('NOT installed');
    expect(line!.text).toContain('already registered');
  });

  it('still says something when the reason is missing, rather than nothing', () => {
    const line = describeHotkey({ chord: 'Alt+F9', installed: false, error: null });
    expect(line!.problem).toBe(true);
    expect(line!.text).toContain('Alt+F9');
  });
});

describe('the config line', () => {
  const app = (overrides: Partial<AppStatus> = {}): AppStatus => ({
    hotkey: { chord: 'Ctrl+F8', installed: true, error: null },
    config_path: 'C:\\Users\\player\\AppData\\Local\\localplay\\config.toml',
    config_exists: true,
    close_hint: 'Closing this window hides localplay in the tray; recording keeps running.',
    ...overrides,
  });

  it('names the file that was read', () => {
    expect(describeConfig(app())).toBe(
      'Config: C:\\Users\\player\\AppData\\Local\\localplay\\config.toml',
    );
  });

  it('says plainly that the example values are in force when there is no file', () => {
    // Presenting an example's defaults as configured values is the dishonesty this line
    // exists to prevent — along with the user not knowing where to create the file.
    const text = describeConfig(app({ config_exists: false }));

    expect(text).toContain('No config.toml at C:\\Users\\player\\AppData\\Local\\localplay\\config.toml');
    expect(text).toContain('config.example.toml are');
    expect(text).toContain('tray');
  });

  it('says nothing before the shell has answered', () => {
    expect(describeConfig(null)).toBeNull();
  });
});

describe('the automatic-recording line', () => {
  it('says nothing before the first status arrives', () => {
    expect(describeAutoRecord(null)).toBeNull();
  });

  it('says automatic recording is off when no watcher is armed', () => {
    expect(describeAutoRecord(status())).toBe('Automatic recording is off.');
  });

  it('says it is watching once auto-record is on', () => {
    expect(describeAutoRecord(status({ watching_games: true }))).toBe(
      'Watching for a game — recording starts on match.',
    );
  });

  it('names the game that triggered the recording', () => {
    expect(
      describeAutoRecord(status({ watching_games: true, matched_game: 'Dota 2' })),
    ).toBe('Dota 2 detected — recording.');
  });
});
