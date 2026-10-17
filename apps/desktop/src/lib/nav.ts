/**
 * Where the window is, and how it moves.
 *
 * There is no URL router: the whole navigation state is one `View` held by `App.svelte`, and
 * every transition is a pure function here so the rules (what Back does, what happens to the
 * open item when it is deleted, when the rail shows a badge) are pinned by tests instead of by
 * clicking through the window.
 */

import { needsAttention } from './health';
import type { Health } from './types';

/** The five destinations on the left rail, in order. */
export const RAIL_VIEWS = ['library', 'sessions', 'storage', 'health', 'settings'] as const;
export type RailView = (typeof RAIL_VIEWS)[number];

export type View =
  | { kind: 'library' }
  | { kind: 'clip'; id: number }
  | { kind: 'sessions' }
  /** The sessions view with one session open in its detail pane. */
  | { kind: 'session'; id: number }
  | { kind: 'storage' }
  | { kind: 'health' }
  | { kind: 'settings' };

export const INITIAL_VIEW: View = { kind: 'library' };

/** The rail destination a view lives under (a detail page lights its parent). */
export function railOf(view: View): RailView {
  switch (view.kind) {
    case 'clip':
      return 'library';
    case 'session':
      return 'sessions';
    default:
      return view.kind;
  }
}

/**
 * A click on a rail item. Another destination opens at its root; the active one returns a
 * clip page to the library, but leaves an open session alone — the sessions view is
 * master–detail, so its selection is part of the view rather than a page above it.
 */
export function navigate(view: View, rail: RailView): View {
  if (rail === 'sessions' && view.kind === 'session') return view;
  return { kind: rail };
}

export const openClip = (id: number): View => ({ kind: 'clip', id });
export const openSession = (id: number): View => ({ kind: 'session', id });

/** Back from a clip page is the library; nothing else has a page above it. */
export function back(view: View): View {
  return view.kind === 'clip' ? { kind: 'library' } : view;
}

/** Whether Esc / Alt+Left would do anything from here. */
export function canGoBack(view: View): boolean {
  return view.kind === 'clip';
}

/**
 * The view after the index changed under it. An open clip or session that is no longer listed
 * was deleted here or by a retention pass, so the window returns to the list it came from
 * instead of showing something that does not exist.
 */
export function reconcile(view: View, clipIds: readonly number[], sessionIds: readonly number[]): View {
  if (view.kind === 'clip' && !clipIds.includes(view.id)) return { kind: 'library' };
  if (view.kind === 'session' && !sessionIds.includes(view.id)) return { kind: 'sessions' };
  return view;
}

/** The dot on the Health item: something on that page needs a look. */
export function healthBadge(health: Health | null): boolean {
  return health !== null && needsAttention(health);
}

export type NavKey =
  | 'ArrowLeft'
  | 'ArrowRight'
  | 'ArrowUp'
  | 'ArrowDown'
  | 'Home'
  | 'End';

/**
 * Which grid cell a key moves focus to; `current` when it would leave the grid. Moving down
 * from a column that is missing in a short last row lands on the last cell, as file managers
 * do, so a key press never strands the user. `-1` for an empty grid.
 */
export function gridIndex(current: number, key: NavKey, count: number, columns: number): number {
  if (count <= 0) return -1;
  const cols = Math.max(1, Math.floor(columns));
  const last = count - 1;
  const at = Math.min(Math.max(current, 0), last);
  switch (key) {
    case 'ArrowLeft':
      return Math.max(at - 1, 0);
    case 'ArrowRight':
      return Math.min(at + 1, last);
    case 'ArrowUp':
      return at - cols >= 0 ? at - cols : at;
    case 'ArrowDown': {
      if (at + cols <= last) return at + cols;
      const lastRowStart = Math.floor(last / cols) * cols;
      return at < lastRowStart ? last : at;
    }
    case 'Home':
      return 0;
    case 'End':
      return last;
  }
}

/** Roving focus in a vertical list (the rail): Up/Down wrap, Home/End jump. */
export function listIndex(current: number, key: NavKey, count: number): number {
  if (count <= 0) return -1;
  const last = count - 1;
  switch (key) {
    case 'ArrowUp':
    case 'ArrowLeft':
      return current <= 0 ? last : current - 1;
    case 'ArrowDown':
    case 'ArrowRight':
      return current >= last ? 0 : current + 1;
    case 'Home':
      return 0;
    case 'End':
      return last;
  }
}
