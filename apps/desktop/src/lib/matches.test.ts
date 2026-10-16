import { describe, expect, it } from 'vitest';
import { canExtractMatch, kda, matchCaveat, matchSpan, matchTitle, scoreboardByTeam } from './matches';
import type { MatchDto, ScoreRow } from './types';

function match(over: Partial<MatchDto> = {}): MatchDto {
  return {
    index: 0,
    start_ms: 1_000,
    end_ms: 5_000,
    complete: true,
    start_inferred: false,
    game_mode: null,
    map: null,
    result: null,
    scoreboard: [],
    events: [],
    ...over,
  };
}

function row(over: Partial<ScoreRow> = {}): ScoreRow {
  return {
    name: 'a',
    champion: 'Ahri',
    team: 'ORDER',
    kills: 5,
    deaths: 1,
    assists: 3,
    cs: 100,
    ward_score: 1,
    is_local: false,
    ...over,
  };
}

describe('matchSpan', () => {
  it('places a match as percentages of the session', () => {
    expect(matchSpan(match(), 10_000)).toEqual({ leftPct: 10, widthPct: 40 });
  });
  it('has no span when the session length is unknown', () => {
    expect(matchSpan(match(), 0)).toBeNull();
  });
  it('never leaves the bar', () => {
    const s = matchSpan(match({ start_ms: -500, end_ms: 99_000 }), 10_000)!;
    expect(s.leftPct).toBe(0);
    expect(s.leftPct + s.widthPct).toBe(100);
  });
});

describe('matchTitle and matchCaveat', () => {
  it('states only what the game reported', () => {
    expect(matchTitle(match())).toBe('Match 1');
    expect(matchTitle(match({ index: 1, game_mode: 'CLASSIC', result: 'Lose' }))).toBe('Match 2 · Classic · Loss');
  });
  it('names inferred boundaries', () => {
    expect(matchCaveat(match())).toBeNull();
    expect(matchCaveat(match({ complete: false, start_inferred: true }))).toContain('start estimated');
    expect(matchCaveat(match({ complete: false }))).toContain('end estimated');
  });
});

describe('scoreboard helpers', () => {
  it('groups by team in first-seen order', () => {
    const groups = scoreboardByTeam([row({ team: 'CHAOS' }), row({ team: 'ORDER' }), row({ team: 'CHAOS' }), row({ team: null })]);
    expect(groups.map((g) => [g.team, g.rows.length])).toEqual([['CHAOS', 2], ['ORDER', 1], ['', 1]]);
  });
  it('formats K/D/A', () => {
    expect(kda(row())).toBe('5/1/3');
  });
  it('refuses to extract an empty match', () => {
    expect(canExtractMatch(match())).toBe(true);
    expect(canExtractMatch(match({ start_ms: 5, end_ms: 5 }))).toBe(false);
  });
});
