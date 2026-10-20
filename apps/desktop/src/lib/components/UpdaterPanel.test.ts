// @vitest-environment jsdom
/**
 * The updates section of the settings panel.
 *
 * The panel is the whole of the feature: one button, one request, one answer. So the
 * tests here are mostly about *when* the request happens — it must not happen because the
 * window opened, and it must not happen twice because the panel was rendered again.
 */

import { mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('@tauri-apps/plugin-updater', () => ({
  check: vi.fn(),
}));

import { check } from '@tauri-apps/plugin-updater';
import type { Update } from '@tauri-apps/plugin-updater';
import UpdaterPanel from './UpdaterPanel.svelte';

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

describe('the updates section', () => {
  let app: ReturnType<typeof mount> | undefined;
  afterEach(() => {
    if (app) void unmount(app);
    app = undefined;
    document.body.innerHTML = '';
    mockedCheck.mockReset();
  });

  async function settle() {
    for (let i = 0; i < 10; i++) {
      await tick();
      await new Promise((r) => setTimeout(r, 0));
    }
  }

  async function mountPanel(): Promise<HTMLElement> {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(UpdaterPanel, { target });
    await settle();
    return target;
  }

  /** The panel's text, with the markup's own line breaks folded away. */
  function text(target: HTMLElement): string {
    return (target.textContent ?? '').replace(/\s+/g, ' ');
  }

  function button(target: HTMLElement, label: string): HTMLButtonElement | null {
    return (
      [...target.querySelectorAll<HTMLButtonElement>('button')].find(
        (b) => b.textContent?.trim() === label,
      ) ?? null
    );
  }

  it('asks nothing when the panel opens', async () => {
    await mountPanel();
    // The panel is on screen with its button; nothing was requested.
    expect(mockedCheck).not.toHaveBeenCalled();
  });

  it('asks once, when the button is pressed', async () => {
    mockedCheck.mockResolvedValue(null);
    const target = await mountPanel();

    button(target, 'Check for updates')?.click();
    await settle();

    expect(mockedCheck).toHaveBeenCalledTimes(1);
  });

  it('says so when there is nothing newer', async () => {
    mockedCheck.mockResolvedValue(null);
    const target = await mountPanel();

    button(target, 'Check for updates')?.click();
    await settle();

    // The panel goes back to its resting state rather than claiming an update it did not
    // find, and the button is still there for a later check.
    expect(text(target)).not.toContain('is available');
    expect(button(target, 'Check for updates')).not.toBeNull();
  });

  it('shows the release it found, with its notes and its date', async () => {
    mockedCheck.mockResolvedValue(fakeUpdate());
    const target = await mountPanel();

    button(target, 'Check for updates')?.click();
    await settle();

    expect(text(target)).toContain('0.3.0');
    expect(text(target)).toContain('you are running 0.2.0');
    expect(text(target)).toContain('Published 2026-10-20');
    expect(text(target)).toContain('A new release.');
    expect(button(target, 'Download and install')).not.toBeNull();
  });

  it('installs only when the user asks for it', async () => {
    const update = fakeUpdate();
    mockedCheck.mockResolvedValue(update);
    const target = await mountPanel();

    button(target, 'Check for updates')?.click();
    await settle();

    // The update was found and nothing was downloaded: that is a second button's job.
    expect(update.downloadAndInstall).not.toHaveBeenCalled();

    button(target, 'Download and install')?.click();
    await settle();

    expect(update.downloadAndInstall).toHaveBeenCalledTimes(1);
  });

  it('says that the application has to be restarted to finish', async () => {
    mockedCheck.mockResolvedValue(fakeUpdate());
    const target = await mountPanel();

    button(target, 'Check for updates')?.click();
    await settle();
    button(target, 'Download and install')?.click();
    await settle();

    expect(text(target)).toContain('Restart replayed to finish');
  });

  it('shows the failure instead of swallowing it', async () => {
    mockedCheck.mockRejectedValue(new Error('the endpoint did not answer'));
    const target = await mountPanel();

    button(target, 'Check for updates')?.click();
    await settle();

    expect(target.querySelector('[role="alert"]')?.textContent).toContain(
      'the endpoint did not answer',
    );
  });

  it('lets the update go, and releases what the backend was holding', async () => {
    const update = fakeUpdate();
    mockedCheck.mockResolvedValue(update);
    const target = await mountPanel();

    button(target, 'Check for updates')?.click();
    await settle();
    button(target, 'Not now')?.click();
    await settle();

    expect(update.close).toHaveBeenCalledTimes(1);
    expect(button(target, 'Download and install')).toBeNull();
  });
});
