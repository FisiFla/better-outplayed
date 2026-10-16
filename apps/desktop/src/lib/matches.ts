/**
 * What the match timeline shows: per-match placement on the session axis, labels, and the
 * League end-of-game scoreboard grouped by team.
 *
 * Kept out of the component for the same reason `markers.ts` is: the part that can be wrong
 * without a screenshot showing it — a match drawn past the end of the bar, a result the game
 * never reported, a scoreboard row grouped under the wrong team.
 */

import { clamp, formatDuration } from './time';
import type { MatchDto, ScoreRow } from './types';

/** A match's span on the session axis, as percentages of the bar. Never outside 0–100. */
export interface MatchSpan {
  leftPct: number;
  widthPct: number;
}

/**
 * Where a match sits on a session of `durationMs`. A session whose length is unknown (`0`)
 * has no axis, so there is no span — `null`, not a fabricated full-width bar.
 */
export function matchSpan(match: Pick<MatchDto, 'start_ms' | 'end_ms'>, durationMs: number): MatchSpan | null {
  if (durationMs <= 0) return null;
  const start = clamp(match.start_ms, 0, durationMs);
  const end = clamp(match.end_ms, start, durationMs);
  return { leftPct: (start / durationMs) * 100, widthPct: ((end - start) / durationMs) * 100 };
}

/** `"Match 2 · Classic · Win"`: only what the game actually reported. */
export function matchTitle(match: MatchDto): string {
  const parts = [`Match ${match.index + 1}`];
  if (match.game_mode !== null && match.game_mode.length > 0) {
    parts.push(match.game_mode.charAt(0) + match.game_mode.slice(1).toLowerCase());
  }
  if (match.result !== null) parts.push(match.result === 'Win' ? 'Win' : match.result === 'Lose' ? 'Loss' : match.result);
  return parts.join(' · ');
}

/** One line of caveats: which boundaries were inferred rather than recorded. */
export function matchCaveat(match: Pick<MatchDto, 'complete' | 'start_inferred'>): string | null {
  const notes: string[] = [];
  if (match.start_inferred) notes.push('start estimated (recording began mid-game)');
  if (!match.complete) notes.push('end estimated (the game did not report its end)');
  return notes.length > 0 ? notes.join('; ') : null;
}

export function matchLength(match: Pick<MatchDto, 'start_ms' | 'end_ms'>): string {
  return formatDuration(Math.max(0, match.end_ms - match.start_ms));
}

export function kda(row: Pick<ScoreRow, 'kills' | 'deaths' | 'assists'>): string {
  return `${row.kills}/${row.deaths}/${row.assists}`;
}

/** Scoreboard rows grouped by team, in first-seen order; a row with no team goes under `''`. */
export function scoreboardByTeam(rows: ScoreRow[]): { team: string; rows: ScoreRow[] }[] {
  const groups: { team: string; rows: ScoreRow[] }[] = [];
  for (const row of rows) {
    const team = row.team ?? '';
    let group = groups.find((g) => g.team === team);
    if (group === undefined) {
      group = { team, rows: [] };
      groups.push(group);
    }
    group.rows.push(row);
  }
  return groups;
}

/** Whether a match can be cut out right now (same refusals as extracting a range). */
export function canExtractMatch(match: Pick<MatchDto, 'end_ms' | 'start_ms'>): boolean {
  return match.end_ms > match.start_ms;
}
