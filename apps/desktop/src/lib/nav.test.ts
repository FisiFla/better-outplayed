import { describe, expect, it } from 'vitest';
import {
  back,
  canGoBack,
  gridIndex,
  healthBadge,
  INITIAL_VIEW,
  listIndex,
  navigate,
  openClip,
  openSession,
  RAIL_VIEWS,
  railOf,
  reconcile,
} from './nav';
import type { Health } from './types';

const healthy: Health = {
  recorder_state: 'idle',
  recorder_error: null,
  encoder: null,
  ffmpeg_available: true,
  disk: { path: '/c', free_bytes: 100 * 1024 ** 3, total_bytes: 500 * 1024 ** 3 },
  event_sources: [],
  recent_errors: [],
  log_path: '/log',
};

describe('rail mapping', () => {
  it('opens on the library', () => {
    expect(INITIAL_VIEW).toEqual({ kind: 'library' });
  });

  it('puts every detail page under its parent destination', () => {
    expect(railOf(openClip(3))).toBe('library');
    expect(railOf(openSession(3))).toBe('sessions');
    for (const rail of RAIL_VIEWS) expect(railOf({ kind: rail })).toBe(rail);
  });

  it('opens another destination at its root', () => {
    expect(navigate(openClip(1), 'storage')).toEqual({ kind: 'storage' });
    expect(navigate(openSession(1), 'settings')).toEqual({ kind: 'settings' });
  });

  it('returns a clip page to the library when its own item is clicked', () => {
    expect(navigate(openClip(1), 'library')).toEqual({ kind: 'library' });
  });

  it('keeps an open session when the sessions item is clicked again', () => {
    expect(navigate(openSession(7), 'sessions')).toEqual(openSession(7));
    expect(navigate({ kind: 'storage' }, 'sessions')).toEqual({ kind: 'sessions' });
  });
});

describe('back', () => {
  it('goes from a clip page to the library and nowhere else', () => {
    expect(back(openClip(1))).toEqual({ kind: 'library' });
    expect(back(openSession(1))).toEqual(openSession(1));
    expect(back({ kind: 'health' })).toEqual({ kind: 'health' });
    expect(canGoBack(openClip(1))).toBe(true);
    expect(canGoBack({ kind: 'library' })).toBe(false);
  });
});

describe('reconcile after the index changed', () => {
  it('returns to the library when the open clip was deleted', () => {
    expect(reconcile(openClip(2), [1, 3], [])).toEqual({ kind: 'library' });
  });

  it('returns to the sessions list when the open session was deleted', () => {
    expect(reconcile(openSession(9), [], [1, 2])).toEqual({ kind: 'sessions' });
  });

  it('leaves a view alone while its item is still listed', () => {
    expect(reconcile(openClip(2), [2], [])).toEqual(openClip(2));
    expect(reconcile(openSession(2), [], [2])).toEqual(openSession(2));
    expect(reconcile({ kind: 'storage' }, [], [])).toEqual({ kind: 'storage' });
  });
});

describe('health badge', () => {
  it('is off until health is read and while healthy', () => {
    expect(healthBadge(null)).toBe(false);
    expect(healthBadge(healthy)).toBe(false);
  });

  it('lights for a failed recorder, missing ffmpeg, low disk or a failed source', () => {
    expect(healthBadge({ ...healthy, recorder_state: 'failed' })).toBe(true);
    expect(healthBadge({ ...healthy, ffmpeg_available: false })).toBe(true);
    expect(healthBadge({ ...healthy, disk: { path: '/c', free_bytes: 1024, total_bytes: 500 * 1024 ** 3 } })).toBe(true);
    expect(
      healthBadge({
        ...healthy,
        event_sources: [{ source: 'gsi', state: 'failed', detail: 'port in use' }],
      }),
    ).toBe(true);
  });
});

describe('grid keyboard navigation (4 columns, 10 cells)', () => {
  const at = (current: number, key: Parameters<typeof gridIndex>[1]) => gridIndex(current, key, 10, 4);

  it('moves one cell left and right, clamped at the ends', () => {
    expect(at(5, 'ArrowRight')).toBe(6);
    expect(at(5, 'ArrowLeft')).toBe(4);
    expect(at(0, 'ArrowLeft')).toBe(0);
    expect(at(9, 'ArrowRight')).toBe(9);
  });

  it('moves a row up and down', () => {
    expect(at(5, 'ArrowDown')).toBe(9);
    expect(at(5, 'ArrowUp')).toBe(1);
    expect(at(1, 'ArrowUp')).toBe(1);
  });

  it('lands on the last cell when the row below is short, and stays on the last row', () => {
    expect(at(7, 'ArrowDown')).toBe(9);
    expect(at(9, 'ArrowDown')).toBe(9);
    expect(at(8, 'ArrowDown')).toBe(8);
  });

  it('jumps with Home and End and copes with an empty grid or one column', () => {
    expect(at(5, 'Home')).toBe(0);
    expect(at(5, 'End')).toBe(9);
    expect(gridIndex(0, 'ArrowDown', 0, 4)).toBe(-1);
    expect(gridIndex(1, 'ArrowDown', 3, 1)).toBe(2);
    expect(gridIndex(0, 'ArrowDown', 3, 0)).toBe(1);
  });
});

describe('rail roving focus', () => {
  it('wraps in both directions and supports Home/End', () => {
    expect(listIndex(0, 'ArrowUp', 5)).toBe(4);
    expect(listIndex(4, 'ArrowDown', 5)).toBe(0);
    expect(listIndex(2, 'ArrowDown', 5)).toBe(3);
    expect(listIndex(2, 'Home', 5)).toBe(0);
    expect(listIndex(2, 'End', 5)).toBe(4);
    expect(listIndex(0, 'ArrowDown', 0)).toBe(-1);
  });
});
