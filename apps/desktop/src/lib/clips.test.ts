import { describe, expect, it } from 'vitest';
import {
  basename,
  describeDelete,
  describeTrim,
  resolveSelection,
  thumbnailAtMs,
  toClipView,
  toClipViews,
} from './clips';
import type { ClipView } from './clips';
import type { ClipDto, DeleteOutcome } from './types';

const assetUrl = (path: string) => `asset://localhost/${encodeURIComponent(path)}`;

function clip(overrides: Partial<ClipDto> = {}): ClipDto {
  return {
    id: 1,
    path: '/home/player/.local/share/localplay/clips/clip-1.mp4',
    started_at_ms: 0,
    duration_ms: 12_345,
    size_bytes: 812 * 1024,
    codec: 'h264_nvenc',
    favourite: false,
    created_at_ms: 1_756_000_000_000,
    ...overrides,
  };
}

describe('basename', () => {
  it('takes the last component of a POSIX path', () => {
    expect(basename('/home/player/clips/clip-1.mp4')).toBe('clip-1.mp4');
  });

  it('takes the last component of a Windows path', () => {
    // The application runs on Windows and indexes the paths it wrote there, while the
    // development host is macOS: a split that only knew about `/` would render every clip's
    // name as its whole path.
    expect(basename('C:\\Users\\player\\Videos\\localplay\\clip.mp4')).toBe('clip.mp4');
    expect(basename('C:\\mixed\\separators/clip.mp4')).toBe('clip.mp4');
  });

  it('leaves a bare file name alone', () => {
    expect(basename('clip.mp4')).toBe('clip.mp4');
    expect(basename('')).toBe('');
  });

  it('does not treat a trailing separator as a name', () => {
    expect(basename('/home/player/clips/')).toBe('/home/player/clips/');
  });
});

describe('thumbnailAtMs', () => {
  it('asks for a frame one second in', () => {
    expect(thumbnailAtMs(60_000)).toBe(1_000);
    expect(thumbnailAtMs(2_000)).toBe(1_000);
  });

  it('asks for the middle frame of anything one second or shorter', () => {
    expect(thumbnailAtMs(1_000)).toBe(500);
    expect(thumbnailAtMs(800)).toBe(400);
    expect(thumbnailAtMs(1)).toBe(0);
  });

  it('never asks for a frame that is not there', () => {
    // The last frame of a stream starts a frame interval before its nominal duration ends,
    // so the choice here is deliberately nowhere near the end.
    expect(thumbnailAtMs(0)).toBe(0);
    expect(thumbnailAtMs(-1_000)).toBe(0);
    expect(thumbnailAtMs(Number.NaN)).toBe(0);
    for (const duration of [1, 250, 1_000, 5_000, 1_800_000]) {
      expect(thumbnailAtMs(duration)).toBeLessThan(duration);
    }
  });
});

describe('toClipView', () => {
  it('maps the row the store holds into what the list shows', () => {
    const view = toClipView(clip(), assetUrl);

    expect(view.id).toBe(1);
    expect(view.name).toBe('clip-1.mp4');
    expect(view.path).toBe('/home/player/.local/share/localplay/clips/clip-1.mp4');
    expect(view.assetUrl).toBe(assetUrl('/home/player/.local/share/localplay/clips/clip-1.mp4'));
    expect(view.durationMs).toBe(12_345);
    expect(view.durationLabel).toBe('0:12.3');
    expect(view.sizeBytes).toBe(812 * 1024);
    expect(view.sizeLabel).toBe('812 KiB');
    expect(view.codec).toBe('h264_nvenc');
    expect(view.favourite).toBe(false);
    expect(view.createdAtMs).toBe(1_756_000_000_000);
    // Local time, so only the shape is pinned.
    expect(view.createdAtLabel).toMatch(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$/);
  });

  it('builds the playback URL through the injected asset-protocol mapping', () => {
    // Playback is the asset protocol and nothing else: the view never hands the raw path to
    // a <video> element, because a bare filesystem path is not something a webview can load.
    const view = toClipView(clip({ path: 'C:\\clips\\a b.mp4' }), assetUrl);
    expect(view.assetUrl).toContain(assetUrl('C:\\clips\\a b.mp4'));
    expect(view.assetUrl).not.toBe(view.path);
  });

  it('carries the favourite flag through', () => {
    expect(toClipView(clip({ favourite: true }), assetUrl).favourite).toBe(true);
  });
});

describe('toClipViews', () => {
  it('preserves the order the store returned', () => {
    // Newest first is decided in Rust, on the wall-clock column, and is not second-guessed
    // here: re-sorting by `started_at_ms` in the UI would silently disagree with it.
    const rows = [
      clip({ id: 3, started_at_ms: 10 }),
      clip({ id: 2, started_at_ms: 99_999 }),
      clip({ id: 1, started_at_ms: 0 }),
    ];
    expect(toClipViews(rows, assetUrl).map((view) => view.id)).toEqual([3, 2, 1]);
  });

  it('maps an empty list to an empty list', () => {
    expect(toClipViews([], assetUrl)).toEqual([]);
  });
});

describe('resolveSelection', () => {
  const views: ClipView[] = toClipViews([clip({ id: 5 }), clip({ id: 4 })], assetUrl);

  it('keeps a selection that is still in the list', () => {
    expect(resolveSelection(views, 4)).toBe(4);
  });

  it('falls back to the newest clip when the selected one is gone', () => {
    // Deleting the clip that was open, or a cleanup pass that removed it, must not leave
    // the detail pane pointing at a row that no longer exists.
    expect(resolveSelection(views, 99)).toBe(5);
  });

  it('selects nothing when there is nothing to select', () => {
    expect(resolveSelection([], null)).toBeNull();
    expect(resolveSelection([], 5)).toBeNull();
  });

  it('picks the newest clip on a first open', () => {
    expect(resolveSelection(views, null)).toBe(5);
  });
});

describe('describeTrim', () => {
  it('reports a trim whose length matches what was asked for', () => {
    const written = clip({
      id: 7,
      path: '/clips/game.trim-1500-3000.mp4',
      duration_ms: 1_500,
    });
    const message = describeTrim({ startMs: 1_500, endMs: 3_000 }, written);

    expect(message).toContain('game.trim-1500-3000.mp4');
    expect(message).toContain('0:01.5');
    expect(message).toContain('clip #7');
    expect(message).toContain('losslessly remuxed');
  });

  it('shows both numbers when a stream copy cut somewhere else', () => {
    // The point of the whole report: a stream copy cannot always cut where it was asked to,
    // and rounding the two numbers into agreement would hide that.
    const written = clip({ id: 8, path: '/clips/cut.mp4', duration_ms: 2_450 });
    const message = describeTrim({ startMs: 0, endMs: 3_000 }, written);

    expect(message).toContain('asked for 0:03.0');
    expect(message).toContain('0:02.5');
    expect(message).toContain('stream copy cuts where the stream allows');
  });

  it('treats a difference within a couple of frames as equal', () => {
    const written = clip({ id: 9, path: '/clips/near.mp4', duration_ms: 3_010 });
    expect(describeTrim({ startMs: 1_000, endMs: 4_000 }, written)).toContain('losslessly remuxed');
  });
});

describe('describeDelete', () => {
  const outcome = (overrides: Partial<DeleteOutcome> = {}): DeleteOutcome => ({
    id: 1,
    row_deleted: true,
    file_removed: true,
    orphaned_path: null,
    already_missing: false,
    bytes_reclaimed: 1_048_576,
    thumbnails_removed: 1,
    ...overrides,
  });

  it('reports a clean delete with what it freed', () => {
    expect(describeDelete(outcome(), 'clip-1.mp4')).toBe('clip-1.mp4 deleted, freeing 1.0 MiB.');
  });

  it('says an id with no row deleted nothing at all', () => {
    const message = describeDelete(outcome({ row_deleted: false, file_removed: false }), 'gone.mp4');
    expect(message).toContain('no longer in the index');
    expect(message).toContain('nothing was deleted');
  });

  it('names the orphaned file when the unlink failed', () => {
    // Spec §8.2: this state is recoverable, but only if it is said out loud — reporting it
    // as a clean delete would leave a file nobody knows about filling the disk.
    const message = describeDelete(
      outcome({ file_removed: false, orphaned_path: '/clips/stuck.mp4', bytes_reclaimed: 0 }),
      'stuck.mp4',
    );
    expect(message).toContain('/clips/stuck.mp4');
    expect(message).toContain('left on disk');
  });

  it('says when the file was already gone', () => {
    const message = describeDelete(outcome({ file_removed: false, already_missing: true }), 'x.mp4');
    expect(message).toContain('already gone');
  });
});
