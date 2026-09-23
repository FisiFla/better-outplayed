/**
 * Fail loudly if the sidecar binaries the installer is about to embed are not there.
 *
 * `bundle.resources` in `src-tauri/tauri.conf.json` maps `<repo>/binaries/` into the
 * bundle's own `binaries/` directory — which is the one `FfmpegBinaries::discover` reads
 * once the app is installed. Tauri copies whatever is in that source directory, including
 * nothing at all: a missing pair would produce a perfectly valid installer whose application
 * finds no ffmpeg and cannot clip. That is the defect this script exists to make impossible
 * to ship quietly, so it runs as `beforeBundleCommand` — after the app is compiled, before
 * any installer is written, and never during `tauri dev`.
 *
 * What it checks: that `binaries/` holds a pair under the names the *host* platform needs
 * (`ffmpeg.exe`/`ffprobe.exe` on Windows, `ffmpeg`/`ffprobe` elsewhere). What it cannot
 * check: that those files are a real, working, correctly-licensed ffmpeg. Only
 * `cargo xtask sidecars fetch` proves that, by verifying the pinned SHA-256 of the archive
 * it extracts from; this script would happily pass a placeholder a developer staged by hand
 * (`docs/packaging.md` says when that is legitimate and when it is not).
 *
 * No dependency is used here: `node:path` and `node:fs` cover it, and the frontend has no
 * build-time code of its own to borrow.
 */

import { readdirSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
// scripts/ -> apps/desktop -> apps -> the repository root, where `xtask sidecars fetch`
// writes. The same directory `tauri.conf.json` maps into the bundle.
const repoRoot = resolve(here, '..', '..', '..');
const sourceDir = join(repoRoot, 'binaries');
const bundleDir = 'binaries';

const exe = (stem) => (process.platform === 'win32' ? `${stem}.exe` : stem);
const required = [exe('ffmpeg'), exe('ffprobe')];

const missing = required.filter((name) => {
  try {
    return !statSync(join(sourceDir, name)).isFile();
  } catch {
    return true;
  }
});

if (missing.length > 0) {
  const fetch = process.platform === 'win32' ? 'x86_64-pc-windows-msvc' : undefined;
  console.error(
    [
      '',
      `error: the installer would be built without ${missing.join(' and ')}.`,
      '',
      `  looked in: ${sourceDir}`,
      `  needs:     ${required.join(', ')}  (host platform: ${process.platform})`,
      '',
      'The bundled sidecars are what lets an installed localplay run on a machine that has',
      'no system ffmpeg, so bundling without them would ship an app that cannot clip.',
      '',
      'Fix it by fetching the pinned sidecar binaries:',
      '',
      `  cargo xtask sidecars fetch${fetch ? ` --target ${fetch}` : ''}`,
      '',
      'On macOS there is no pinned sidecar yet (spec §2: the capture path is Windows-only,',
      'and `xtask/sidecars.toml` pins a Windows archive), so a macOS bundle needs a real',
      'arm64 LGPL ffmpeg placed in binaries/ by hand -- or `--no-bundle` to build the',
      'application without an installer. See docs/packaging.md.',
      '',
    ].join('\n'),
  );
  process.exit(1);
}

// `ls`-sized evidence in the build log: which files, how big, and where they will land.
const sizes = required.map((name) => {
  const bytes = statSync(join(sourceDir, name)).size;
  return `${name} (${(bytes / 1024 / 1024).toFixed(1)} MiB)`;
});
console.log(
  `bundling sidecars from ${sourceDir}: ${sizes.join(', ')} -> <bundle>/Contents/Resources/${bundleDir} ` +
    '(macOS) or the installation directory (Windows)',
);

// Anything else in binaries/ is copied too, and a stray file is worth seeing in the log
// rather than discovered in an installer.
const extra = readdirSync(sourceDir).filter(
  (name) => !required.includes(name) && name !== '.gitkeep',
);
if (extra.length > 0) {
  console.log(`note: binaries/ also holds ${extra.join(', ')} -- bundled as well`);
}
