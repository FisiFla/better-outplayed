// @vitest-environment jsdom
/**
 * The health panel's diagnostics button.
 *
 * The panel is a readout, and this is the one thing a user does *with* a readout: hand it
 * to somebody else. What is under test is that the copy says what was copied, that it says
 * plainly that nothing left the machine — this application has no telemetry, and a button
 * called "diagnostics" that quietly uploaded something would be the opposite of that — and
 * that a failed copy is a failure and not a copy.
 */

import { mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';

import HealthPanel from './HealthPanel.svelte';
import type { ClipSource, DiagnosticsReport } from '../ipc';
import type { Health } from '../types';

const health: Health = {
  recorder_state: 'idle',
  recorder_error: null,
  encoder: null,
  ffmpeg_available: true,
  disk: null,
  event_sources: [],
  recent_errors: [],
  log_path: 'C:\\Users\\player\\AppData\\Local\\replayed\\logs\\replayed.log',
};

const REPORT: DiagnosticsReport = {
  version: '0.2.0',
  platform: 'windows 11 x86_64',
  generated_at: '2026-10-20 09:00:00',
  config: '[encode]\nvendor = "auto"\nbitrate_kbps = 20000',
  problems: ['ffmpeg was not found on PATH'],
  log_tail: 'ERROR the encoder could not be started',
};

describe('the health panel', () => {
  let app: ReturnType<typeof mount> | undefined;
  afterEach(() => {
    if (app) void unmount(app);
    app = undefined;
    document.body.innerHTML = '';
  });

  async function settle() {
    for (let i = 0; i < 10; i++) {
      await tick();
      await new Promise((r) => setTimeout(r, 0));
    }
  }

  async function mountPanel(source: ClipSource): Promise<HTMLElement> {
    const target = document.createElement('div');
    document.body.appendChild(target);
    app = mount(HealthPanel, { target, props: { health, source } });
    await settle();
    return target;
  }

  function copyButton(target: HTMLElement): HTMLButtonElement | null {
    return (
      [...target.querySelectorAll<HTMLButtonElement>('button')].find(
        (b) => b.textContent?.trim() === 'Copy diagnostics',
      ) ?? null
    );
  }

  /**
   * The panel's text, with the markup's own line breaks folded away.
   *
   * The sentences are wrapped in the source, so the raw `textContent` of a paragraph is
   * several lines with the indentation in between; a test that reads them has to fold the
   * whitespace the way a reader's eye does.
   */
  function text(target: HTMLElement): string {
    return (target.textContent ?? '').replace(/\s+/g, ' ');
  }

  it('has a button that copies the report', async () => {
    const target = await mountPanel({
      copyDiagnostics: () => Promise.resolve(REPORT),
    } as unknown as ClipSource);
    expect(copyButton(target), 'the copy button').not.toBeNull();
  });

  it('says what was copied once the copy is done', async () => {
    const target = await mountPanel({
      copyDiagnostics: () => Promise.resolve(REPORT),
    } as unknown as ClipSource);

    copyButton(target)!.click();
    await settle();

    // The confirmation itself, not the whole panel: the report behind the disclosure
    // repeats these facts, and the point is that the *confirmation* names them.
    const said = target.querySelector('[role="status"]')?.textContent?.replace(/\s+/g, ' ') ?? '';
    expect(said).toContain('Copied to the clipboard');
    expect(said).toContain('0.2.0');
    expect(said).toContain('windows 11 x86_64');
    expect(said).toContain('2026-10-20 09:00:00');
  });

  it('says that nothing left the machine', async () => {
    const target = await mountPanel({
      copyDiagnostics: () => Promise.resolve(REPORT),
    } as unknown as ClipSource);
    const said = text(target);
    expect(said).toContain('Nothing is sent anywhere');
    expect(said).toContain('clipboard');
  });

  it('shows the report it copied, so the user can see what they are pasting', async () => {
    const target = await mountPanel({
      copyDiagnostics: () => Promise.resolve(REPORT),
    } as unknown as ClipSource);
    copyButton(target)!.click();
    await settle();

    const details = target.querySelector('details');
    expect(details, 'the report is behind a disclosure').not.toBeNull();
    const report = (details?.textContent ?? '').replace(/\s+/g, ' ');
    expect(report).toContain('ffmpeg was not found on PATH');
    expect(report).toContain('bitrate_kbps = 20000');
    expect(report).toContain('the encoder could not be started');
  });

  it('shows the failure instead of pretending the copy worked', async () => {
    const target = await mountPanel({
      copyDiagnostics: () => Promise.reject({ code: 'io', message: 'the clipboard is locked' }),
    } as unknown as ClipSource);

    copyButton(target)!.click();
    await settle();

    expect(target.querySelector('[role="alert"]')?.textContent).toContain(
      'the clipboard is locked',
    );
    expect(text(target)).not.toContain('Copied to the clipboard');
  });
});
