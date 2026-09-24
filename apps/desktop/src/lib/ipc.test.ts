/**
 * The IPC layer, driven against a mocked Tauri runtime.
 *
 * The Tauri `invoke`/`convertFileSrc` calls are the one boundary in the frontend that
 * neither TypeScript nor Rust can check for the other side. A command renamed in Rust, or
 * an argument sent as `startMs` where the command declares `start_ms`, fails at runtime —
 * in a window nobody is watching. So this file asserts the boundary itself:
 *
 * 1. the exact command names and argument keys that go over the wire, and
 * 2. that `ipc.ts` and `src-tauri/src/lib.rs` agree — the names match the
 *    `generate_handler!` list, and the argument keys match the Rust parameters, which are
 *    read straight out of the Rust source.
 */

import { readFileSync } from 'node:fs';
import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(async () => ({})),
  convertFileSrc: vi.fn((path: string) => `asset://localhost/${encodeURIComponent(path)}`),
}));

import { convertFileSrc, invoke } from '@tauri-apps/api/core';
import { errorCode, errorMessage, isCommandError, tauriIpc } from './ipc';

const mockedInvoke = vi.mocked(invoke);
const mockedConvert = vi.mocked(convertFileSrc);

const LIB_RS = readFileSync(new URL('../../src-tauri/src/lib.rs', import.meta.url), 'utf8');

/** The command names registered in `generate_handler![...]`. */
function registeredCommands(): string[] {
  const block = /generate_handler!\[([\s\S]*?)\]/.exec(LIB_RS)?.[1] ?? '';
  return block
    .split(',')
    .map((name) => name.trim())
    .filter((name) => name.length > 0);
}

/** The parameter names of a `#[tauri::command]` function, without its `State` argument. */
function rustParameters(command: string): string[] {
  const body = new RegExp(`fn ${command}\\(([\\s\\S]*?)\\)\\s*->`).exec(LIB_RS)?.[1] ?? '';
  return [...body.matchAll(/(\w+)\s*:/g)]
    .map((match) => match[1] ?? '')
    .filter((name) => name !== 'state');
}

/** Run one IPC call and return the first argument `invoke` was handed. */
async function wireName(call: () => Promise<unknown>): Promise<string> {
  mockedInvoke.mockClear();
  await call();
  const [name] = mockedInvoke.mock.calls[0] ?? [];
  return String(name);
}

/** Run one IPC call and return the second argument `invoke` was handed. */
async function wireArgs(call: () => Promise<unknown>): Promise<Record<string, unknown>> {
  mockedInvoke.mockClear();
  await call();
  const [, args] = mockedInvoke.mock.calls[0] ?? [];
  return (args ?? {}) as Record<string, unknown>;
}

beforeEach(() => {
  mockedInvoke.mockClear();
  mockedConvert.mockClear();
});

describe('the command names', () => {
  it('calls each command by the name the Rust side registers', async () => {
    expect(await wireName(() => tauriIpc.listClips())).toBe('list_clips');
    expect(await wireName(() => tauriIpc.storageStats())).toBe('storage_stats');
    expect(await wireName(() => tauriIpc.setFavourite(1, true))).toBe('set_favourite');
    expect(await wireName(() => tauriIpc.trimClip(1, 0, 100))).toBe('trim_clip');
    expect(await wireName(() => tauriIpc.thumbnail(1, 0))).toBe('thumbnail');
    expect(await wireName(() => tauriIpc.deleteClip(1))).toBe('delete_clip');
    expect(await wireName(() => tauriIpc.listSessions())).toBe('list_sessions');
    expect(await wireName(() => tauriIpc.sessionDetail(1))).toBe('session_detail');
    expect(await wireName(() => tauriIpc.sessionEvents(1))).toBe('session_events');
    expect(await wireName(() => tauriIpc.setSessionFavourite(1, true))).toBe(
      'set_session_favourite',
    );
    expect(await wireName(() => tauriIpc.deleteSession(1))).toBe('delete_session');
    expect(await wireName(() => tauriIpc.extractClip(1, 0, 1_000))).toBe('extract_clip');
    expect(await wireName(() => tauriIpc.startRecording())).toBe('start_recording');
    expect(await wireName(() => tauriIpc.stopRecording())).toBe('stop_recording');
    expect(await wireName(() => tauriIpc.recordingStatus())).toBe('recording_status');
    expect(await wireName(() => tauriIpc.clipNow())).toBe('clip_now');
    expect(await wireName(() => tauriIpc.appStatus())).toBe('app_status');
  });

  it('uses every command the Rust side registers, and invents none', async () => {
    // The commands the methods above actually reached, compared against the handler list
    // in src-tauri/src/lib.rs. A command added on one side and not the other fails here
    // rather than as "Command not found" in a window.
    const reached = [
      await wireName(() => tauriIpc.listClips()),
      await wireName(() => tauriIpc.storageStats()),
      await wireName(() => tauriIpc.setFavourite(1, true)),
      await wireName(() => tauriIpc.trimClip(1, 0, 100)),
      await wireName(() => tauriIpc.thumbnail(1, 0)),
      await wireName(() => tauriIpc.deleteClip(1)),
      await wireName(() => tauriIpc.listSessions()),
      await wireName(() => tauriIpc.sessionDetail(1)),
      await wireName(() => tauriIpc.sessionEvents(1)),
      await wireName(() => tauriIpc.setSessionFavourite(1, true)),
      await wireName(() => tauriIpc.deleteSession(1)),
      await wireName(() => tauriIpc.extractClip(1, 0, 1_000)),
      await wireName(() => tauriIpc.startRecording()),
      await wireName(() => tauriIpc.stopRecording()),
      await wireName(() => tauriIpc.recordingStatus()),
      await wireName(() => tauriIpc.clipNow()),
      await wireName(() => tauriIpc.appStatus()),
    ];

    expect([...reached].sort()).toEqual([...registeredCommands()].sort());
  });
});

describe('the argument keys', () => {
  it('sends snake_case, because the commands declare rename_all = "snake_case"', async () => {
    // Tauri's default is camelCase for command arguments; these commands opt out of it, so
    // `start_ms` — not `startMs` — is what the Rust side expects. This is exactly the kind
    // of mismatch that no compiler catches.
    expect(await wireArgs(() => tauriIpc.trimClip(3, 1_000, 2_000))).toEqual({
      id: 3,
      start_ms: 1_000,
      end_ms: 2_000,
    });
    expect(await wireArgs(() => tauriIpc.thumbnail(3, 1_000))).toEqual({
      id: 3,
      at_ms: 1_000,
    });
  });

  it('sends exactly the parameters each Rust command declares', async () => {
    // The recording commands take none: every one of them is answered from the shell's own
    // state, so their arguments are the empty set and the Rust side must declare no
    // parameters either.
    const sent = {
      set_favourite: await wireArgs(() => tauriIpc.setFavourite(3, true)),
      trim_clip: await wireArgs(() => tauriIpc.trimClip(3, 1_000, 2_000)),
      thumbnail: await wireArgs(() => tauriIpc.thumbnail(3, 1_000)),
      delete_clip: await wireArgs(() => tauriIpc.deleteClip(3)),
      list_sessions: await wireArgs(() => tauriIpc.listSessions()),
      session_detail: await wireArgs(() => tauriIpc.sessionDetail(3)),
      session_events: await wireArgs(() => tauriIpc.sessionEvents(3)),
      set_session_favourite: await wireArgs(() => tauriIpc.setSessionFavourite(3, true)),
      delete_session: await wireArgs(() => tauriIpc.deleteSession(3)),
      extract_clip: await wireArgs(() => tauriIpc.extractClip(3, 1_000, 2_000)),
      start_recording: await wireArgs(() => tauriIpc.startRecording()),
      stop_recording: await wireArgs(() => tauriIpc.stopRecording()),
      recording_status: await wireArgs(() => tauriIpc.recordingStatus()),
      clip_now: await wireArgs(() => tauriIpc.clipNow()),
    };

    for (const [command, args] of Object.entries(sent)) {
      const declared = rustParameters(command);
      expect(Object.keys(args).sort(), `arguments for ${command}`).toEqual(declared.sort());
    }
  });

  it('sends no arguments to the commands that take none', async () => {
    expect(mockedInvoke).toHaveBeenCalledTimes(0);
    await tauriIpc.listClips();
    expect(mockedInvoke).toHaveBeenCalledWith('list_clips');
    await tauriIpc.storageStats();
    expect(mockedInvoke).toHaveBeenCalledWith('storage_stats');
    // The recording commands take no arguments either: the shell already knows its
    // application data directory and its config file.
    await tauriIpc.startRecording();
    expect(mockedInvoke).toHaveBeenCalledWith('start_recording');
    await tauriIpc.stopRecording();
    expect(mockedInvoke).toHaveBeenCalledWith('stop_recording');
    await tauriIpc.recordingStatus();
    expect(mockedInvoke).toHaveBeenCalledWith('recording_status');
    await tauriIpc.clipNow();
    expect(mockedInvoke).toHaveBeenCalledWith('clip_now');
    // `app_status` takes nothing either: the shell knows its own config path and hotkey.
    await tauriIpc.appStatus();
    expect(mockedInvoke).toHaveBeenCalledWith('app_status');
  });
});

describe('assetUrl', () => {
  it('turns a path into an asset-protocol URL rather than a file URL', () => {
    const path = 'C:\\Users\\player\\AppData\\Local\\localplay\\clips\\clip-1.mp4';
    expect(tauriIpc.assetUrl(path)).toBe(mockedConvert.mock.results[0]?.value);
    expect(mockedConvert).toHaveBeenCalledWith(path);
    expect(tauriIpc.assetUrl(path)).not.toContain('file://');
  });
});

describe('error handling', () => {
  it('recognises a structured command error', () => {
    const err = { code: 'invalid_range', message: 'the trim range is empty' };
    expect(isCommandError(err)).toBe(true);
    expect(errorCode(err)).toBe('invalid_range');
    expect(errorMessage(err)).toBe('the trim range is empty');
  });

  it('unwraps the message from every shape a rejection arrives in', () => {
    expect(errorMessage('Command not found')).toBe('Command not found');
    expect(errorMessage(new Error('boom'))).toBe('boom');
    expect(errorMessage(null)).toContain('unknown reason');
  });

  it('does not mistake some other object for one of ours', () => {
    expect(isCommandError({ message: 'no code here' })).toBe(false);
    expect(isCommandError('a string')).toBe(false);
    expect(isCommandError(null)).toBe(false);
    expect(errorCode({ message: 'no code here' })).toBeNull();
  });

  it('falls back to something readable for an object it does not know', () => {
    expect(errorMessage({ unexpected: true })).toBe('{"unexpected":true}');
  });
});
