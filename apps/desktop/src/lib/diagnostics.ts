/**
 * Send the frontend's errors to the application's log file.
 *
 * A shipped window has no console anyone can read. The app starts from the Start Menu, a
 * JavaScript error or a failed `invoke` disappears into a devtools panel nobody opens, and the
 * only report that reaches a maintainer is a screenshot. The backend already writes to
 * `%LOCALAPPDATA%\localplay\logs\better-outplayed.log`; this puts the frontend's failures on the
 * same stream, so one file answers what went wrong.
 *
 * Best effort by construction: `invoke` can be the very thing that is failing, so nothing here
 * throws, and a failure while reporting a failure does not recurse.
 */
import { invoke } from '@tauri-apps/api/core';

let reporting = false;

function report(level: 'error' | 'warn' | 'info', message: string, detail: string): void {
  if (reporting) {
    return;
  }
  reporting = true;
  void Promise.resolve(invoke('log_from_frontend', { level, message, detail }))
    .catch(() => {})
    .finally(() => {
      reporting = false;
    });
}

function describe(value: unknown): string {
  if (value instanceof Error) {
    return `${value.message}\n${value.stack ?? '(no stack)'}`;
  }
  if (typeof value === 'string') {
    return value;
  }
  try {
    return JSON.stringify(value) ?? String(value);
  } catch {
    return String(value);
  }
}

/**
 * Route the window's uncaught failures into the log. Called once, before the app is mounted, so
 * that a failure during mounting is caught too.
 */
export function installDiagnostics(): void {
  globalThis.addEventListener('error', (event) => {
    const where = event.filename ? `${event.filename}:${event.lineno}:${event.colno}` : '(no location)';
    report('error', `uncaught error: ${event.message}`, `${where}\n${describe(event.error)}`);
  });

  globalThis.addEventListener('unhandledrejection', (event) => {
    report('error', 'unhandled promise rejection', describe(event.reason));
  });

  const original = console.error.bind(console);
  console.error = (...args: unknown[]): void => {
    original(...args);
    report('error', 'console.error', args.map(describe).join(' '));
  };
}
