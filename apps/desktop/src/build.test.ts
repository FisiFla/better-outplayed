import { mkdtempSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { build } from 'vite';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';

/**
 * Principle 1 (zero network) as a build check: fonts and icons ship inside `dist/`, so the
 * built stylesheets may not reference any remote URL (the CSP forbids it anyway), and the
 * bundled typefaces must actually be emitted.
 */
describe('production build', () => {
  const outDir = mkdtempSync(join(tmpdir(), 'localplay-build-'));
  let assets: string[] = [];

  beforeAll(async () => {
    await build({ logLevel: 'silent', build: { outDir, emptyOutDir: true, sourcemap: false } });
    assets = readdirSync(join(outDir, 'assets'));
  }, 60_000);

  afterAll(() => rmSync(outDir, { recursive: true, force: true }));

  it('references no remote URL from any stylesheet', () => {
    const css = assets.filter((name) => name.endsWith('.css'));
    expect(css.length).toBeGreaterThan(0);
    for (const name of css) {
      const text = readFileSync(join(outDir, 'assets', name), 'utf8');
      expect(text.match(/https?:\/\/[^\s)'"]*/g) ?? []).toEqual([]);
    }
  });

  it('bundles the Inter and JetBrains Mono woff2 files', () => {
    for (const family of ['inter-latin', 'jetbrains-mono-latin']) {
      expect(assets.some((name) => name.startsWith(family) && name.endsWith('.woff2'))).toBe(true);
    }
  });
});
