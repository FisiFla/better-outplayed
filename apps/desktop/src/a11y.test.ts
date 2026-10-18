// @vitest-environment jsdom
import axe from 'axe-core';
import { mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import App from './App.svelte';
import type { ClipSource } from './lib/ipc';
import type { ClipDto, RecordingStatus, SessionDto, SettingsDto, StorageStats } from './lib/types';

// The real module reaches for the Tauri runtime, which does not exist under jsdom.
vi.mock('@tauri-apps/api/core', () => ({
  convertFileSrc: (p: string) => `asset://${p}`,
  invoke: () => Promise.resolve(undefined),
}));

const clip: ClipDto = {
  id: 1, path: '/c/a.mp4', started_at_ms: 0, duration_ms: 30_000, size_bytes: 5_000_000,
  codec: 'h264', favourite: false, title: null, game: 'Valorant', tags: [], created_at_ms: 1_700_000_000_000,
};
const session: SessionDto = {
  id: 1, game: 'Valorant', mode: 'session', started_at_ms: 1_700_000_000_000,
  ended_at_ms: 1_700_000_600_000, final_path: '/s/1.mp4', size_bytes: 9_000_000,
  favourite: false, scratch_dir: '/s/scratch', duration_ms: 600_000,
};
const stats: StorageStats = {
  clip_count: 1, total_bytes: 5_000_000, missing_count: 0, favourite_count: 0, favourite_bytes: 0,
  cap_bytes: 10_000_000_000, max_age_days: 30, cap_met: true, over_cap_by_bytes: 0,
  planned_deletions: 0, bytes_after: 5_000_000, clips_dir: '/c', warnings: [],
};
const settings: SettingsDto = {
  clips_dir: '/c', clips_dir_resolved: '/c', fps: 60, output_size: '1080p', mic_enabled: false,
  mic_device: '', output_device: '', save_session: false,
  auto_record: true, watch_titles: ['VALORANT'], defaulted: [], config_exists: true,
  config_path: '/cfg/config.toml', bitrate_kbps: 8000, quality: 'balanced', capture_monitor: 0,
  capture_window: '',
};
const recording: RecordingStatus = {
  running: false, frames: 0, segments: 0, bytes: 0, span_ms: 0, dropped: 0, dropped_audio: 0,
  skipped: 0, fps: 0, configured_fps: 60, effective_fps: 0, drift_ms: 0, clips: 0, error: null,
  watching_games: true, matched_game: null, recorder_mode: 'buffer', mic_enabled: false,
};

/** Explicit answers for what the window reads on open; any other command resolves to nothing. */
function fakeSource(): ClipSource {
  const known: Record<string, unknown> = {
    listClips: () => Promise.resolve([clip]),
    storageStats: () => Promise.resolve(stats),
    listSessions: () => Promise.resolve([session]),
    sessionEvents: () => Promise.resolve([]),
    sessionDetail: () => Promise.resolve(session),
    getSettings: () => Promise.resolve(settings),
    recordingStatus: () => Promise.resolve(recording),
    appStatus: () =>
      Promise.resolve({
        hotkey: { chord: 'Ctrl+Shift+C', installed: true, error: null },
        config_path: '/cfg/config.toml', config_exists: true, close_hint: 'Closing hides to the tray.',
      }),
    thumbnail: () => Promise.resolve({ clip_id: 1, path: '/t.jpg', at_ms: 0, cached: true }),
    health: () =>
      Promise.resolve({
        recorder_state: 'idle', recorder_error: null, encoder: null, ffmpeg_available: true,
        disk: null, event_sources: [], recent_errors: [], log_path: '/cfg/app.log',
      }),
    assetUrl: (p: string) => `asset://${p}`,
  };
  return new Proxy(known, {
    get: (t, k: string) => t[k] ?? (() => Promise.resolve(undefined)),
  }) as unknown as ClipSource;
}

async function settle() {
  for (let i = 0; i < 10; i++) {
    await tick();
    await new Promise((r) => setTimeout(r, 0));
  }
}

async function violations(root: HTMLElement) {
  const result = await axe.run(root, {
    // jsdom does no layout or painting, so contrast cannot be computed.
    rules: { 'color-contrast': { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.nodes.map((n) => n.target.join(' ')).join(', ')}`);
}

describe('accessibility (axe)', () => {
  let app: ReturnType<typeof mount> | undefined;
  afterEach(() => {
    if (app) void unmount(app);
    app = undefined;
    document.body.innerHTML = '';
  });

  it('the main window has no axe violations', async () => {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(App, { target, props: { source: fakeSource() } });
    await settle();
    // Guard against a vacuous pass: the data really rendered.
    expect(target.textContent).toContain('Valorant');
    expect(await violations(target)).toEqual([]);
  });

  /** Mount the window, open one destination from the rail, and return its root. */
  async function openView(label: string) {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(App, { target, props: { source: fakeSource() } });
    await settle();
    const item = [...target.querySelectorAll<HTMLButtonElement>('nav button')].find((b) =>
      b.textContent?.includes(label),
    );
    expect(item, `rail item ${label}`).toBeDefined();
    item!.click();
    await settle();
    expect(item!.getAttribute('aria-current')).toBe('page');
    return target;
  }

  it.each([
    ['Sessions', 'session-list'],
    ['Storage', 'storage'],
    ['Health', 'health'],
    ['Settings', 'settings'],
  ])('the %s view has no axe violations', async (label, region) => {
    const target = await openView(label);
    // Not vacuous: the region for this view really is on screen and has content.
    const el = target.querySelector(`[data-region="${region}"]`);
    expect(el?.textContent?.trim().length ?? 0).toBeGreaterThan(0);
    expect(await violations(target)).toEqual([]);
  });

  it('a clip page has no axe violations', async () => {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(App, { target, props: { source: fakeSource() } });
    await settle();
    target.querySelector<HTMLButtonElement>('[data-region="clip-grid"] button.row')?.click();
    await settle();
    expect(target.querySelector('[data-region="clip-page"]')).not.toBeNull();
    expect(await violations(target)).toEqual([]);
  });

  it('the harness does catch a violation', async () => {
    const target = document.createElement('div');
    target.innerHTML = '<img src="x.png"><button></button>';
    document.body.appendChild(target);
    const found = await violations(target);
    expect(found.some((v) => v.startsWith('image-alt'))).toBe(true);
    expect(found.some((v) => v.startsWith('button-name'))).toBe(true);
  });
});
