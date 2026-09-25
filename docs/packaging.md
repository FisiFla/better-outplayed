# Packaging localplay

**Status: packagable, not shippable.** Two things were true before this document existed, and
neither is true now: `bundle.active` was `false`, so `tauri build` produced no artifact anyone
could install; and the ffmpeg sidecars went into the repository's `binaries/` directory, which
an installed application never looks at, so a shipped build would have fallen through to `PATH`
and refused to start on a machine without a system ffmpeg — defeating the entire sidecar
pipeline.

What is still **not** true is more important than what is, and §5 lists it in full: the
artifacts are **unsigned**, the **icon is a placeholder**, there is **no auto-update**, and
**nobody has installed this on Windows**. Read §5 before repeating anything in §1 as a
release claim.

This document is written the same way as
[`verification-status.md`](verification-status.md): every claim is tagged with how it is known
— *built here*, *read from the Tauri source*, or *not verified*.

---

## 1. What is built, and what was actually built

| Target | Config | Artifact | Built in this repository? |
|---|---|---|---|
| Windows | `bundle.targets: ["nsis"]` (`src-tauri/tauri.windows.conf.json`) | `localplay_0.1.0_x64-setup.exe` | **No** — needs a Windows host (§3) |
| macOS | `bundle.targets: ["app"]` (`src-tauri/tauri.macos.conf.json`) | `localplay.app` | **Yes**, on this development host (§4) |
| macOS | (not enabled) `.dmg` | `localplay_0.1.0_aarch64.dmg` | **No** — `hdiutil` is refused in this environment (§4.3) |
| Linux | — | — | No target: the capture path is Windows-only (spec §2) |

Common metadata, from `apps/desktop/src-tauri/tauri.conf.json`, all of it consistent with the
rest of the repository rather than invented here:

| Field | Value | Where it comes from |
|---|---|---|
| `productName` | `localplay` | the README's own name for the app; `bundle.licenseFile` aside, this is what the installer shows |
| `mainBinaryName` | `localplay` | without it the installed executable would be `localplay-desktop`, the Cargo package name |
| `version` | `0.1.0` | `[workspace.package] version` in the root `Cargo.toml`, and `package.json` |
| `identifier` | `io.github.fisifla.better-outplayed` | reverse-DNS of the project's owner (`github.com/FisiFla`, the maintainer's address in the git history). Fixed now on purpose: it is the Windows uninstall key and the macOS bundle id |
| `publisher` | `FisiFla` | the copyright holder in `LICENSE-MIT` and the GitHub account that owns the repository |
| `copyright` | `Copyright (c) 2026 FisiFla` | verbatim from `LICENSE-MIT` |
| `category` | `Video` | the app records and cuts video; maps to `public.app-category.video` on macOS |
| `homepage` | `https://github.com/FisiFla/better-outplayed` | `repository` in the root `Cargo.toml` |
| `shortDescription` | *Local, zero-cloud game clipping and replay buffer for Windows* | the README's tagline |
| `longDescription` | the README's first paragraph, plus the Windows/encoder requirement | README + spec §2 |
| `licenseFile` | **unset, deliberately** | localplay is dual-licensed (MIT **OR** Apache-2.0) and this key takes a single path. Naming one file would misstate the licence. A release should add a short file that states the dual licence and points at both, or the installer simply shows no licence page |

---

## 2. How the bundled ffmpeg reaches an installed app

This is the part that decides whether a shipped build works at all, so it is documented
end to end.

### 2.1 The sidecars come from `binaries/`, through Tauri's resource mapping
`cargo xtask sidecars fetch` downloads the pinned ffmpeg archive, verifies its SHA-256 against
`xtask/sidecars.toml`, and extracts `ffmpeg`/`ffprobe` into the repository's `binaries/`
(gitignored — see §2.4 for why a `.gitkeep` inside it *is* tracked).

`apps/desktop/src-tauri/tauri.conf.json` then hands that directory to the bundler:

```json
"resources": { "../../../binaries/": "binaries/" }
```

The relative path is resolved from `src-tauri/`, so `../../../binaries/` is the repository's
sidecar directory, and the destination is the bundle's `<resource dir>/binaries/`. This is the
map form of `bundle.resources` (Tauri v2: *"To fine control where the files will get copied
to, use a map instead"*), and the directory form means the destination keeps the file names
exactly as fetched: a `.exe` on Windows, a bare `ffmpeg` elsewhere.

### 2.2 Where that lands, per platform

`<resource dir>` is whatever Tauri's `resource_dir()` resolves to, and it is **not the same
directory** on both platforms (Tauri's own documentation of that method, and the installer
templates):

| Platform | Resource directory | Sidecars therefore at | Installer mechanism |
|---|---|---|---|
| Windows (`nsis`, `msi`) | the directory holding the executable | `%LOCALAPPDATA%\localplay\binaries\ffmpeg.exe` (per-user) or `C:\Program Files\localplay\binaries\ffmpeg.exe` (all-users) | the NSIS template does `SetOutPath $INSTDIR` and then `File /a "/oname=<destination>"` for every resource |
| macOS (`.app`) | `<Foo>.app/Contents/Resources` | `localplay.app/Contents/Resources/binaries/ffmpeg` | `bundle_project` copies resources into `Contents/Resources` and the main binary into `Contents/MacOS` |

### 2.3 How the app finds them (`crates/media/src/binaries.rs`)

`FfmpegBinaries::discover` searches, in this order, and takes the first directory holding both
binaries:

1. **an explicit directory**, if the caller named one (nothing in the application does today);
2. **next to the executable** — `<exe dir>/binaries/`. On Windows this is the installation
   directory, i.e. exactly where the installer's resource mapping puts them;
3. **inside the installed bundle's resources** — `<exe dir>/../Resources/binaries/`, which is
   the macOS `.app` layout. This is the candidate that did not exist before, and without it a
   packaged macOS app never looks inside itself;
4. **a checkout's `binaries/`**, found by walking up from the executable to the first directory
   with a `Cargo.toml` — the *development-only* fallback that a `cargo run` executable needs,
   since it lives in `target/<profile>/`;
5. **`PATH`**, last, because a system ffmpeg is the one source localplay cannot vouch for (the
   LGPL-only, hardware-encoder-only assumptions of spec §2 do not hold for an arbitrary build).

Everything an installer can produce outranks everything a developer's machine can produce, and
the failure message names every location it searched, in order.

### 2.4 Why `binaries/.gitkeep` is tracked

`tauri-build` resolves every `bundle.resources` entry at compile time and **fails the build**
when one does not exist. A mapping into a gitignored directory would therefore make the desktop
crate uncompilable on any host that has not fetched the sidecars — including CI, which never
fetches them (the pinned archive is Windows-only). A zero-byte, tracked `binaries/.gitkeep`
keeps the directory present everywhere; the `.gitignore` rules that make that possible are
commented in place, because git cannot re-include a file inside an excluded *directory*.

### 2.5 Why bundling refuses to run without the sidecars

Tauri copies whatever is in the source directory — including nothing. A missing pair would
produce a perfectly valid installer whose application cannot clip, discovered by a user rather
than by us. So `build.beforeBundleCommand` runs `npm run sidecars:check`
(`apps/desktop/scripts/require-sidecars.mjs`), which fails the bundling step — after the app is
compiled, before any installer is written — if `binaries/` does not hold the pair under the
names the host platform needs, and prints the sizes and destination of what it is about to
embed. It is not run by `tauri dev`.

What that check does **not** do: prove the files are a real, working, correctly-licensed
ffmpeg. Only `cargo xtask sidecars fetch` proves that, by verifying the archive's pinned hash.
§4.2 is the case where this matters. It also does not stop a *second* platform's sidecars being
bundled: the mapping copies the whole directory, so a checkout holding both the fetched Windows
pair and a macOS one embeds both (§4.1 shows the 201 MiB difference that makes). The check
prints what else it found rather than failing on it, because a developer working across both
platforms from one checkout is a normal thing to be doing.

---

## 3. Building the Windows installer (the shipping target)

Run on a Windows 10 1903+/Windows 11 x64 host with the Rust MSVC toolchain, Node.js 20+, and a
hardware encoder for anything that records (none is needed to build):

```powershell
# 1. The sidecars: pinned and checksum-verified, extracted into binaries\ (gitignored).
cargo xtask sidecars fetch --target x86_64-pc-windows-msvc

# 2. Frontend dependencies, exactly as CI installs them.
cd apps\desktop
npm ci

# 3. Compile and bundle. `-t/--target` is optional on a native x64 host.
npm run tauri -- build --target x86_64-pc-windows-msvc
```

Expected artifacts (the paths and names follow Tauri's conventions; **neither was produced
here**, because the installer cannot be built from macOS):

```text
apps\desktop\src-tauri\target\x86_64-pc-windows-msvc\release\bundle\nsis\localplay_0.1.0_x64-setup.exe
apps\desktop\src-tauri\target\x86_64-pc-windows-msvc\release\bundle\msi\localplay_0.1.0_x64_en-US.msi   # only with --bundles msi
```

**This is a compile-and-bundle step and nothing else. It does not launch the application.**

For an MSI as well, build with `npm run tauri -- build --bundles nsis,msi` (or add `"msi"` to
`tauri.windows.conf.json`). MSI is opt-in because WiX v3 can only run on Windows, the build needs
the Windows VBSCRIPT optional feature, and the result is a per-machine install that prompts for
elevation — none of which suits a consumer clipping utility. NSIS gives a single `.exe`
installer with a per-user install and no UAC prompt.

### 3.1 What the NSIS installer does

Read from Tauri v2's NSIS template (`crates/tauri-bundler/src/bundle/windows/nsis/installer.nsi`),
not from a run:

- installs the executable and every resource to `$INSTDIR`, per-user by default:
  `%LOCALAPPDATA%\localplay` (all-users, `C:\Program Files\localplay`, is available in the UI
  and needs elevation). `tauri.windows.conf.json` pins `installMode: currentUser` explicitly so
  this is a decision rather than an inherited default;
- creates a Start Menu shortcut (and an optional desktop shortcut offered by the finish page),
  and an Add/Remove Programs entry (`DisplayName`, `Publisher`, `InstallLocation`, `DisplayIcon`)
  plus `uninstall.exe`;
- **checks and installs the WebView2 runtime if it is missing**, which by default means
  downloading Microsoft's bootstrapper — see §5.4;
- refuses to install while localplay is running, and reuses the existing installation record
  when it is run over an existing one (a non-NSIS install of the same product is detected and
  the user is asked to remove it first).

### 3.2 What it does not do

- It does not install ffmpeg system-wide, and does not touch `PATH`. The bundled copy is the
  app's private one — that is the point of the sidecar pipeline;
- it registers no file associations and no URL protocols (none are configured, and the asset
  protocol is an internal Tauri scheme);
- it installs no service, scheduled task or run-at-startup entry (Tauri's elevated update task
  is not enabled — there is no updater to drive it);
- it does not check for a GPU or a hardware encoder. localplay fails loudly at *runtime* when
  the machine cannot encode (spec §3.2), which is deliberate;
- it has no licence page (§1).

### 3.3 Two Windows-specific things a release has to decide

Both are read out of the NSIS template; neither has been observed on Windows.

1. **The install directory is also the application's data directory** in the default per-user
   mode: `%LOCALAPPDATA%\localplay` holds the release binary *and* `localplay.db`, `clips\` and
   `thumbnails\`. Nothing breaks — the app never writes into the installation directory, the
   installer only owns the exe and `binaries\`, and uninstall cannot delete a non-empty
   directory — but mixing product files with a user's recordings in one folder is a wart. Fix
   by either moving the app's data under a subdirectory of `%LOCALAPPDATA%\localplay` (a
   `localplay-recorder`/`store` change, and a migration for any existing install), or by giving
   the installer a distinct install directory.
2. **The uninstaller's "delete app data" checkbox does not delete localplay's data.** It
   removes `%APPDATA%\<identifier>` and `%LOCALAPPDATA%\<identifier>` — Tauri's directories,
   which this app does not use. The checkbox is therefore misleading in the *safe* direction:
   clips and the index survive an uninstall. A release should either point it at
   `%LOCALAPPDATA%\localplay` through an NSIS hook, or drop the checkbox and say plainly that
   recordings are never deleted by the uninstaller.

### 3.4 Checking an installer without installing it

On the Windows box, the installer's contents can be listed without running either it or the
app (`7z l localplay_0.1.0_x64-setup.exe`, 7-Zip understands NSIS archives) — useful as a
first-pass check that `binaries\ffmpeg.exe` is inside. `npm run sidecars:check` already prints
what was embedded at build time, and that *is* verified here (§4.2).

---

## 4. Building on macOS (verification only)

macOS is the development host, **not** a shipping target: spec §2 rules out non-Windows capture,
and no macOS ffmpeg is pinned. The `.app` exists so the packaging path can be built and
inspected on the machine the work is done on.

```bash
cargo xtask sidecars fetch --target x86_64-pc-windows-msvc   # optional here
cd apps/desktop
npm ci
npm run tauri -- build            # -> src-tauri/target/release/bundle/macos/localplay.app
```

### 4.1 What was built here

The command above, on this host (exit status 0), with the host pair staged by hand (§4.2):

```console
$ cd apps/desktop && CARGO_HOME="$PWD/../../target/cargo-home" npm run tauri -- build
       Built application at: .../apps/desktop/src-tauri/target/release/localplay
     Running beforeBundleCommand `npm run sidecars:check`

> localplay-desktop@0.1.0 sidecars:check
> node scripts/require-sidecars.mjs

bundling sidecars from .../localplay/binaries: ffmpeg (0.0 MiB), ffprobe (0.0 MiB)
    Bundling localplay.app (.../bundle/macos/localplay.app)
    Finished 1 bundle at:
        .../apps/desktop/src-tauri/target/release/bundle/macos/localplay.app
```

Artifact: **`apps/desktop/src-tauri/target/release/bundle/macos/localplay.app`**, ad-hoc signed
only (§4.4). Its size is decided entirely by what sits in `binaries/` when it is built:

| `binaries/` held | `.app` size | Of which sidecars |
|---|---|---|
| the host pair and `.gitkeep` | 13,064 KiB (12.8 MiB) | 1,004 bytes of placeholders |
| the host pair *and* the fetched Windows pair | **214 MB** | 105,423,872 + 105,221,120 bytes of `.exe` files that macOS never runs |

The second row is what a checkout that has fetched the Windows sidecars produces — including
this one while the verification above was run — and the build log says so: the check prints
`note: binaries/ also holds ffmpeg.exe, ffprobe.exe -- bundled as well`, because the mapping
copies the **whole directory**. The `.exe` files are inert on macOS, but they are 201 MiB of
ballast in an installer, so a real Windows build must not be staged with a macOS pair either.
The listing below is from the first, smaller build:

```console
$ find localplay.app -type f -exec ls -l {} \; | awk '{print $5, $9}'
1135    localplay.app/Contents/Info.plist
13244960 localplay.app/Contents/MacOS/localplay
0       localplay.app/Contents/Resources/binaries/.gitkeep
242     localplay.app/Contents/Resources/binaries/ffmpeg
243     localplay.app/Contents/Resources/binaries/ffprobe
117998  localplay.app/Contents/Resources/icon.icns
```

`Contents/Resources/binaries/{ffmpeg,ffprobe}` is exactly where `discover` looks once the
executable is at `Contents/MacOS/`, and the executable bit survived the copy — relevant if a
real macOS ffmpeg is ever bundled. (The zero-byte `.gitkeep` is copied along with them: it
exists so that `tauri-build` can resolve the directory in a fresh clone, §2.4, and discovery
ignores it.) `Info.plist` carries the metadata from §1
(`CFBundleIdentifier=io.github.fisifla.better-outplayed`, `CFBundleShortVersionString=0.1.0`,
`LSApplicationCategoryType=public.app-category.video`, `NSHumanReadableCopyright=Copyright (c)
2026 FisiFla`).

### 4.2 What those sidecars are — read this before quoting the listing above

They are **placeholders**: two shell scripts (a few hundred bytes) that print
`placeholder ffmpeg: not a real ffmpeg` and exit non-zero. There is no pinned macOS ffmpeg to
put there (`xtask/sidecars.toml` pins a Windows archive, and a Homebrew ffmpeg is GPL, which
localplay does not ship against), and `npm run sidecars:check` says so in the build log —
`ffmpeg (0.0 MiB)`. So the listing above proves **placement**, and nothing about shipping a
working binary. The Windows build embeds the real, checksum-verified pair, because that is the
platform the pipeline exists for.

They were staged by hand, into the same `<repo>/binaries/` the fetch command writes:

```sh
for stem in ffmpeg ffprobe; do
  printf '#!/bin/sh\necho "placeholder %s: not a real ffmpeg" >&2\nexit 1\n' "$stem" > "binaries/$stem"
  chmod +x "binaries/$stem"
done
```

Without them the build stops before writing anything, which is the point of the check:

```console
$ npm run sidecars:check
error: the installer would be built without ffmpeg and ffprobe.
  looked in: .../localplay/binaries
  needs:     ffmpeg, ffprobe  (host platform: darwin)
  ...
$ echo $?
1
```

Delete them again before running `cargo test`: `binaries/` is the development fallback that
`FfmpegBinaries::discover` consults *before* `PATH`, so a placeholder left in place is handed to
the CLI's integration tests, which fail with
`using encoder 'libx264': placeholder ffmpeg: not a real ffmpeg`. That is the fallback working
as designed — loudly — and it is how this note came to be written.

The lookup itself is tested against real bytes: `crates/media`'s
`installed_app_finds_the_sidecars_in_its_own_bundle_resources` builds the same tree and asserts
discovery returns the resource copy, and — with a bundle that actually exists —
`LOCALPLAY_APP_BUNDLE=…/localplay.app cargo test -p localplay-media --lib
a_real_app_bundle_beats_path` resolves the *real* `.app`'s `Contents/MacOS` through the
production code path and asserts it lands on `Contents/Resources/binaries/ffmpeg`, ahead of the
real ffmpeg on this host's `PATH`.

### 4.3 The `.dmg` could not be built here

`--bundles dmg` was attempted and **failed**, for an environment reason rather than a project
one: Tauri's bundler runs its `bundle_dmg.sh`, which shells out to `hdiutil`, and this session
is not permitted to create disk images.

```console
$ ./bundle_dmg.sh --volname localplay ... localplay_0.1.0_aarch64.dmg localplay.app
Creating disk image...
could not access /Volumes/localplay - Operation not permitted
hdiutil: create failed - Operation not permitted
failed to bundle project: error running bundle_dmg.sh
```

`tauri.macos.conf.json` therefore lists `["app"]`, so the default macOS build succeeds. On a
host where `hdiutil` is permitted, `npm run tauri -- build --bundles app,dmg` should produce
`target/release/bundle/dmg/localplay_0.1.0_aarch64.dmg`; that is **unverified**.

### 4.4 The macOS bundle is not distributable

```console
$ codesign -dv --verbose=4 localplay.app
Identifier=localplay_desktop-f15df2dfe1af8f63
CodeDirectory ... flags=0x20002(adhoc,linker-signed)
Signature=adhoc
Info.plist=not bound

$ spctl -a -vv localplay.app
localplay.app: code has no resources but signature indicates they must be present
```

The Mach-O binary carries the ad-hoc signature the linker applies, but the bundle was never
signed as a bundle, so the resources added after linking are not sealed and Gatekeeper's
assessment fails. No signing identity or notarisation is configured, and Tauri skipped the
signing step. **The app was not launched to find out what Gatekeeper does with it** — see §7.

---

## 5. What is not done, and blocks a real release

| # | Gap | Consequence | Where it has to be fixed |
|---|---|---|---|
| 5.1 | **No code signing.** Neither the Windows installer nor the macOS app is signed with a code-signing certificate | Windows SmartScreen shows "unknown publisher" and hides the app behind *More info → Run anyway*; macOS Gatekeeper refuses the `.app` outright (§4.4) | a Windows Authenticode certificate for NSIS/MSI, an Apple Developer ID + notarisation for macOS (`bundle.macOS.signingIdentity`, `APPLE_CERTIFICATE`/`APPLE_ID` for notarisation) |
| 5.2 | **No notarisation**, macOS-only, and the same certificate work | an unsigned `.app` cannot be distributed at all, only run locally | the macOS signing flow in Tauri's documentation; needs a paid Apple Developer account |
| 5.3 | **No auto-update mechanism.** `bundle.createUpdaterArtifacts` is off and the Tauri updater plugin is not installed | every release is a manual download; a signed updater is *not possible* until 5.1–5.2 are done, since an unsigned update is exactly the attack the signature exists to prevent. Spec §15 lists auto-update as out of scope, so this is deliberate — but it is what a user will notice first | `@tauri-apps/plugin-updater`, a signing keypair, and a release host |
| 5.4 | **The installer may need the network.** Tauri's default WebView2 mode is `downloadBootstrapper` | on a machine with no WebView2 (a bare Windows 10 1903–2004 install) the installer downloads Microsoft's bootstrapper. The *application* stays zero-network, but the install does not. Win11 and up-to-date Win10 machines are unaffected | `bundle.windows.webviewInstallMode`: `offlineInstaller` (embeds the runtime, +~127 MB on top of the ~201 MiB of sidecars) or `skip` (a documented requirement instead). A product decision — the project's principle 1 argues for offline, installer size argues against |
| 5.5 | **The icon is a placeholder** (§6) | it is a teal play triangle on near-black, generated at scaffold time; shipping it would look unfinished | real artwork, then `npm run tauri -- icon` |
| 5.6 | **No uninstaller verification on Windows** — nobody has installed, upgraded or uninstalled localplay | §3.1–3.3 are read out of the template, not observed: a broken uninstaller, a file left in `$INSTDIR`, a Start Menu shortcut that survives, or a data-deleting bug would all be found by the first user | install → run → uninstall → reinstall on the Windows box, and correct §3.3 |
| 5.7 | **No CI packaging job.** CI runs tests, the Windows cross-check and the frontend; it never runs `tauri build` | a change that breaks bundling is caught by hand or not at all | a `macos-latest` job that builds the `.app`, and (once a Windows runner is acceptable to the project) one that builds the NSIS installer |
| 5.8 | **The `.app` has never been launched**, and the installer has never been run | everything in §4 is a statement about the artifact as *files* | a normal login session on the two platforms, which is out of scope for a headless task (§7) |

Not on this list, because it is done: the sidecars *are* bundled, and discovery *does* look
where they land (§2, §4.1–4.2).

---

## 6. The icon

`apps/desktop/src-tauri/icons/` now holds a full desktop set generated from the placeholder
that shipped with the Tauri scaffold:

```bash
cd apps/desktop
npm run tauri -- icon src-tauri/icons/app-icon.png
```

- `app-icon.png` is the **source artwork**: 512×512, RGBA, a teal play triangle inside a teal
  ring on the dark `#0b0c10` background the window already uses. It is a placeholder.
- The command generated `32x32.png`, `64x64.png`, `128x128.png`, `128x128@2x.png`, `icon.icns`
  (macOS), `icon.ico` (Windows, six layers), `icon.png`, and the `Square*Logo`/`StoreLogo` set
  for Microsoft Store packaging. All of them are committed, so a checkout builds without
  running the generator.
- `bundle.icon` lists the five PNG/ICNS/ICO files the two shipping platforms actually need.
  A test in `apps/desktop/src-tauri/src/lib.rs` fails if any listed file is missing, because a
  wrong path there breaks the bundle build.
- The mobile outputs the generator also writes (`android/`, `ios/`) were **not** committed:
  localplay has no mobile target.

**Real artwork is a product decision, not a task.** Whatever replaces `app-icon.png` should be a
square, transparent-cornered PNG of at least 1024×1024 with a dark-friendly silhouette, and a
designer's own choice of mark — the current one is a placeholder nothing else in the product
depends on.

---

## 7. What the build and the tests do not touch

localplay's development host is also a gaming machine, running kernel-level anti-cheat, and its
owner may be mid-game. So, stated explicitly:

- **Building an installer is a compile-and-bundle step.** It runs `cargo build`, the Vite
  frontend build, `npm run sidecars:check`, and (on Windows) NSIS or WiX. It does **not** launch
  the application — `npm run tauri -- build` never does; that is `tauri dev`'s job.
- **Installing and running are separate, later, human decisions.** Nothing here requires them,
  and §5.6/5.8 record them as not done.
- **No part of the packaging path captures the screen, enumerates windows, or synthesises
  input.** The only occurrences of the names to grep for in shipping code are two comment
  lines that disclaim their use:

  ```console
  $ grep -rn "keybd_event\|SendInput\|SendKeys" apps crates xtask
  xtask/src/verify.rs:22://! nothing enumerates windows.** There is no `keybd_event`, no `SendInput`, no
  xtask/src/verify.rs:23://! `SendKeys`, no `mouse_event`, and no enumeration call anywhere in this crate. The
  ```

  The same disclaimer is in the Phase 1 runbook, and it holds for the build: `tauri build`
  compiles, signs nothing, copies files, and writes an installer.
- The `.dmg` attempt in §4.3 is the only system tool this work invoked outside the build; the
  environment refused it, nothing was mounted, and nothing was written outside the ignored
  `target/` directories (and `binaries/`).

---

## 8. What this document does not prove

- That an **installed Windows application finds and runs its ffmpeg**. The installer was not
  built (it cannot be, from macOS), installed or run. What is proven is the mechanism on both
  sides: the bundler puts the resources in the directory Tauri's own `resource_dir()`
  documents — the executable's directory on Windows, `Contents/Resources` inside a macOS
  `.app` — and discovery reads that directory, ahead of the checkout fallback and `PATH`, with
  tests that build the layout and one that resolves a real `.app` (§4.2).
- That the **macOS bundle is usable**: its sidecars are placeholders, it is unsigned, and it was
  never launched.
- That the **installer's runtime behaviour** matches §3.1–3.3: those paragraphs are read from
  the NSIS template.
- That the app is **shippable**. It is packagable, unsigned, with a placeholder icon.
