/**
 * What the window says after `update_settings` answers.
 *
 * Pure, for the usual reason: the notice wording is a display rule the headless suite
 * pins rather than a screenshot. The restart sentence arrives whole from the backend
 * (`restart_reason` names the keys); this only decides the saved-vs-nothing part.
 */

import type { UpdateSettingsOutcome } from './types';

export function describeSettingsUpdate(outcome: UpdateSettingsOutcome): string {
  const saved =
    outcome.applied.length === 0
      ? 'No changes.'
      : `Saved ${outcome.applied.join(', ')}.`;
  return outcome.restart_reason === null ? saved : `${saved} ${outcome.restart_reason}`;
}
