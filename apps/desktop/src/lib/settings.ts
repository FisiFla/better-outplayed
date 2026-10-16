/**
 * What the window says after `update_settings` answers.
 *
 * Pure, for the usual reason: the notice wording is a display rule the headless suite
 * pins rather than a screenshot. The restart sentence arrives whole from the backend
 * (`restart_reason` names the keys); this only decides the saved-vs-nothing part.
 */

import type { SettingsDto, SettingsUpdate, UpdateSettingsOutcome } from './types';

export function describeSettingsUpdate(outcome: UpdateSettingsOutcome): string {
  const saved =
    outcome.applied.length === 0
      ? 'No changes.'
      : `Saved ${outcome.applied.join(', ')}.`;
  return outcome.restart_reason === null ? saved : `${saved} ${outcome.restart_reason}`;
}

/** The quality presets, mirroring the backend table (`QUALITY_PRESETS` in config.rs). */
export const QUALITY_OPTIONS = [
  { value: 'low', label: 'Low (8 Mbit/s)' },
  { value: 'balanced', label: 'Balanced (20 Mbit/s)' },
  { value: 'high', label: 'High (40 Mbit/s)' },
] as const;

/** A monitor index is a whole number from 0; the backend refuses one with no monitor behind it. */
export function monitorIndex(text: string): number | null {
  return /^\d+$/.test(text.trim()) ? Number(text.trim()) : null;
}

/**
 * The capture update for the drafts: only what differs from what is saved, so applying an
 * untouched panel writes nothing and a `custom` bitrate is never overwritten by a preset the
 * user did not pick.
 */
export function captureUpdate(
  saved: SettingsDto,
  draft: { quality: string; monitor: number; window: string },
): SettingsUpdate {
  const update: SettingsUpdate = {};
  if (draft.quality !== saved.quality && draft.quality !== 'custom') update.quality = draft.quality;
  if (draft.monitor !== saved.capture_monitor) update.capture_monitor = draft.monitor;
  if (draft.window.trim() !== saved.capture_window) update.capture_window = draft.window.trim();
  return update;
}
