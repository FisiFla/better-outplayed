/**
 * The clip-source contract, between the interface and the screenshot harness.
 *
 * `ClipSource` is the seam the window is driven through: `ipc.ts` implements it against
 * the Tauri runtime, and the screenshot harness implements it against a fixture. Both
 * implementations are plain objects, so **neither the compiler nor the type checker can
 * tell that one of them has fallen behind the interface** — a method added to
 * `ClipSource` is a missing method in a fake until the moment something calls it, and
 * then it is a `TypeError` inside a window, or a `pageerror` in a screenshot run that
 * nobody was watching.
 *
 * That is not hypothetical. `chooseClipsDir` was added to the interface and to the real
 * implementation, but not to the harness's fake; the screenshot run then failed with
 * `chooseClipsDir is not a function` in the settings scenario. The harness is not a CI
 * gate, so the README's "20 screenshots, 227 checks, 0 failures" went stale instead of
 * turning red.
 *
 * So this file asserts the harness's fake implements every method the interface
 * declares. It reads both out of the source, the way `ipc.test.ts` reads the Rust
 * handler list out of `lib.rs` — the interface is a type, so it is erased at run time
 * and cannot be introspected.
 */

import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

const IPC_TS = readFileSync(new URL('./ipc.ts', import.meta.url), 'utf8');
const SHOTS_MJS = readFileSync(new URL('../../scripts/shots.mjs', import.meta.url), 'utf8');

/** The method names `ClipSource` declares. */
function interfaceMethods(): string[] {
  const block = /export interface ClipSource \{([\s\S]*?)\n\}/.exec(IPC_TS)?.[1] ?? '';
  // Every method is declared as `name(...)`, so the identifier before the parenthesis
  // is the name. A doc-comment line starts with `*`, which is not an identifier.
  return [...block.matchAll(/^\s*(\w+)\s*\(/gm)].map((match) => match[1] ?? '');
}

/** The method names the screenshot harness's fake source implements. */
function fakeMethods(): string[] {
  const start = SHOTS_MJS.indexOf('globalThis.__localplayClipSource = {');
  expect(start, 'the harness must install a fake clip source').toBeGreaterThan(-1);
  const block = SHOTS_MJS.slice(start, SHOTS_MJS.indexOf('\n  };', start));
  const names = new Set<string>();
  for (const match of block.matchAll(/^\s+async (\w+)\(/gm)) names.add(match[1] ?? '');
  // `assetUrl` is the one member that is a plain arrow property rather than a method.
  for (const match of block.matchAll(/^\s{4}(\w+):\s*\(/gm)) names.add(match[1] ?? '');
  return [...names];
}

describe('the screenshot harness implements the clip-source contract', () => {
  it('finds the methods to compare, so a broken parse cannot pass silently', () => {
    // A non-vacuity guard: if either regex stopped matching, the comparison below would
    // compare two empty sets and pass.
    const declared = interfaceMethods();
    expect(declared.length).toBeGreaterThan(30);
    expect(declared).toContain('listClips');
    expect(declared).toContain('chooseClipsDir');
    expect(fakeMethods().length).toBeGreaterThan(30);
  });

  it('implements every method the interface declares', () => {
    const implemented = new Set(fakeMethods());
    const missing = interfaceMethods().filter((name) => !implemented.has(name));
    expect(
      missing,
      `the screenshot harness's fake clip source is missing ${missing.join(', ')}; ` +
        'the window would throw where it calls them',
    ).toEqual([]);
  });
});
