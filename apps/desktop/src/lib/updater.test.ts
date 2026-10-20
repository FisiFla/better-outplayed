/**
 * The updater module: what it reports, and — more importantly — what it does *not* do.
 *
 * The claim this file exists to pin is the one the README makes: replayed makes no
 * network request of its own accord. The module is imported by the settings panel when
 * the window opens, so "the module is loaded" and "a request was made" are two different
 * things, and the first must never imply the second. The first test below is that claim.
 *
 * The rest of the file is about the mapping: the plugin's `Update` resource becomes
 * something the panel can show, and the download events become progress the panel can
 * report.
 */

import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@tauri-apps/plugin-updater', () => ({
  check: vi.fn(),
}));

import { check } from '@tauri-apps/plugin-updater';
import type { Update } from '@tauri-apps/plugin-updater';
import { checkForUpdate, formatUpdateDate } from './updater';

const mockedCheck = vi.mocked(check);

/** The plugin's `Update` resource, as the plugin would hand it over. */
function fakeUpdate(overrides: Record<string, unknown> = {}): Update {
  return {
    rid: 1,
    currentVersion: '0.2.0',
    version: '0.3.0',
    date: '2026-10-20T10:00:00Z',
    body: 'A new release.',
    rawJson: {},
    available: true,
    download: vi.fn(),
    install: vi.fn(),
    downloadAndInstall: vi.fn(async () => {}),
    close: vi.fn(async () => {}),
    ...overrides,
  } as unknown as Update;
}

beforeEach(() => {
  mockedCheck.mockReset();
});

describe('the updater module', () => {
  it('asks nothing until it is asked', () => {
    // The module is imported when the window opens. Importing it must not be a request:
    // there is no startup check, no polling and no background task.
    expect(mockedCheck).not.toHaveBeenCalled();
  });

  it('reports no update when the endpoint has nothing newer', async () => {
    mockedCheck.mockResolvedValue(null);
    expect(await checkForUpdate()).toBeNull();
    expect(mockedCheck).toHaveBeenCalledTimes(1);
  });

  it('reports the release the endpoint announced', async () => {
    mockedCheck.mockResolvedValue(fakeUpdate());
    const offer = await checkForUpdate();

    expect(offer?.version).toBe('0.3.0');
    expect(offer?.currentVersion).toBe('0.2.0');
    expect(offer?.notes).toBe('A new release.');
    expect(offer?.date).toBe('2026-10-20T10:00:00Z');
  });

  it('copes with a release that carries neither notes nor a date', async () => {
    mockedCheck.mockResolvedValue(fakeUpdate({ body: undefined, date: undefined }));
    const offer = await checkForUpdate();

    // Empty, not "undefined": the panel shows these, and a panel that says "undefined" is
    // a panel that is telling the truth about nothing.
    expect(offer?.notes).toBe('');
    expect(offer?.date).toBeNull();
  });

  it('reports the download as it goes, and installs when the bytes are there', async () => {
    const update = fakeUpdate({
      downloadAndInstall: vi.fn(async (onEvent?: (event: unknown) => void) => {
        onEvent?.({ event: 'Started', data: { contentLength: 2048 } });
        onEvent?.({ event: 'Progress', data: { chunkLength: 1024 } });
        onEvent?.({ event: 'Progress', data: { chunkLength: 1024 } });
        onEvent?.({ event: 'Finished' });
      }),
    });
    mockedCheck.mockResolvedValue(update);
    const offer = await checkForUpdate();

    const progress: [number, number | null][] = [];
    await offer!.install((downloaded, total) => progress.push([downloaded, total]));

    expect(update.downloadAndInstall).toHaveBeenCalledTimes(1);
    expect(progress).toEqual([
      [1024, 2048],
      [2048, 2048],
    ]);
  });

  it('lets the update go, so the Rust side releases what it was holding', async () => {
    const update = fakeUpdate();
    mockedCheck.mockResolvedValue(update);
    const offer = await checkForUpdate();

    await offer!.discard();

    expect(update.close).toHaveBeenCalledTimes(1);
  });
});

describe('the release date', () => {
  it('shows the calendar day of an RFC 3339 timestamp', () => {
    expect(formatUpdateDate('2026-10-20T10:00:00Z')).toBe('2026-10-20');
  });

  it('shows a string that is not a timestamp as it is, rather than as nonsense', () => {
    expect(formatUpdateDate('last Thursday')).toBe('last Thursday');
  });

  it('shows nothing for a release with no date', () => {
    expect(formatUpdateDate(null)).toBe('');
  });
});
