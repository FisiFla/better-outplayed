/**
 * The settings notice: what the panel reports after an apply.
 *
 * Same reasoning as the other `*.test.ts` files in this directory: the window cannot
 * be opened here, so the words it shows are pinned here.
 */

import { describe, expect, it } from 'vitest';
import { describeSettingsUpdate } from './settings';
import type { SettingsDto, UpdateSettingsOutcome } from './types';

function settings(overrides: Partial<SettingsDto> = {}): SettingsDto {
  return {
    clips_dir: '',
    clips_dir_resolved: 'C:\\Users\\player\\localplay\\clips',
    fps: 60,
    output_size: '',
    mic_enabled: false,
    auto_record: false,
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
