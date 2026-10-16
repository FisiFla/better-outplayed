/**
 * The settings notice: what the panel reports after an apply.
 *
 * Same reasoning as the other `*.test.ts` files in this directory: the window cannot
 * be opened here, so the words it shows are pinned here.
 */

import { describe, expect, it } from 'vitest';
import { captureUpdate, describeSettingsUpdate, monitorIndex } from './settings';
import type { SettingsDto, UpdateSettingsOutcome } from './types';

function settings(overrides: Partial<SettingsDto> = {}): SettingsDto {
  return {
    clips_dir: '',
    clips_dir_resolved: 'C:\\Users\\player\\localplay\\clips',
    fps: 60,
    output_size: '',
    mic_enabled: false,
    auto_record: false,
    bitrate_kbps: 20000,
    quality: 'balanced',
    capture_monitor: 0,
    capture_window: '',
    watch_titles: ['League of Legends'],
    defaulted: [],
    config_exists: true,
    config_path: 'C:\\Users\\player\\localplay\\config.toml',
    ...overrides,
  };
}

function outcome(overrides: Partial<UpdateSettingsOutcome> = {}): UpdateSettingsOutcome {
  return {
    settings: settings(),
    applied: [],
    restart_required: false,
    restart_reason: null,
    ...overrides,
  };
}

describe('the settings notice', () => {
  it('says nothing changed when nothing did', () => {
    expect(describeSettingsUpdate(outcome())).toBe('No changes.');
  });

  it('names the keys that were saved', () => {
    expect(
      describeSettingsUpdate(outcome({ applied: ['encode.fps', 'mic.enabled'] })),
    ).toBe('Saved encode.fps, mic.enabled.');
  });

  it('carries the restart sentence the backend wrote', () => {
    expect(
      describeSettingsUpdate(
        outcome({
          applied: ['encode.fps'],
          restart_required: true,
          restart_reason: 'a recording is running: encode.fps takes effect now',
        }),
      ),
    ).toBe('Saved encode.fps. a recording is running: encode.fps takes effect now');
  });
});

describe('the capture drafts', () => {
  const saved = settings();
  const same = { quality: 'balanced', monitor: 0, window: '' };

  it('writes nothing for an untouched panel', () => {
    expect(captureUpdate(saved, same)).toEqual({});
  });

  it('sends only the keys that changed, trimming the window title', () => {
    expect(captureUpdate(saved, { quality: 'high', monitor: 1, window: '  League  ' })).toEqual({
      quality: 'high',
      capture_monitor: 1,
      capture_window: 'League',
    });
  });

  it('never overwrites a custom bitrate with the placeholder option', () => {
    const custom = settings({ quality: 'custom', bitrate_kbps: 12000 });
    expect(captureUpdate(custom, { quality: 'custom', monitor: 0, window: '' })).toEqual({});
  });

  it('clears the window with an empty title', () => {
    const windowed = settings({ capture_window: 'Game' });
    expect(captureUpdate(windowed, { ...same, window: '' })).toEqual({ capture_window: '' });
  });

  it('parses a monitor index only as a whole non-negative number', () => {
    expect(monitorIndex(' 2 ')).toBe(2);
    expect(monitorIndex('-1')).toBeNull();
    expect(monitorIndex('1.5')).toBeNull();
    expect(monitorIndex('')).toBeNull();
  });
});
