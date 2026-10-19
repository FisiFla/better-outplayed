# Third-party notices

better-outplayed is a Rust workspace with a Svelte frontend and it ships no third-party
code of its own. Everything below is either compiled into the application, loaded by the
window out of `dist/`, or installed beside the application as a sidecar — and this file
says what each of those is, and under which licence.

The two halves travel differently, so they are listed separately:

- **[Frontend assets](#frontend-assets)** are compiled into `dist/` and loaded by the
  window; nothing is fetched at run time.
- **[Bundled binaries and libraries](#bundled-binaries-and-libraries)** are compiled into
  the application, or installed next to it.

The application's *own* licence is not in this file. better-outplayed is MIT **OR**
Apache-2.0, at your option, and that is stated in `LICENSE.txt` — which the installer
shows on its licence page, and which is installed next to this one.

## Frontend assets

### Inter (variable) - SIL Open Font License 1.1

Shipped via `@fontsource-variable/inter` (latin and latin-ext subsets).

```
Copyright 2016 The Inter Project Authors (https://github.com/rsms/inter) Inter-Italic[opsz,wght].ttf: Copyright 2016 The Inter Project Authors (https://github.com/rsms/inter)

This Font Software is licensed under the SIL Open Font License, Version 1.1.
This license is copied below, and is also available with a FAQ at:
http://scripts.sil.org/OFL


-----------------------------------------------------------
SIL OPEN FONT LICENSE Version 1.1 - 26 February 2007
-----------------------------------------------------------

PREAMBLE
The goals of the Open Font License (OFL) are to stimulate worldwide
development of collaborative font projects, to support the font creation
efforts of academic and linguistic communities, and to provide a free and
open framework in which fonts may be shared and improved in partnership
with others.

The OFL allows the licensed fonts to be used, studied, modified and
redistributed freely as long as they are not sold by themselves. The
fonts, including any derivative works, can be bundled, embedded,
redistributed and/or sold with any software provided that any reserved
names are not used by derivative works. The fonts and derivatives,
however, cannot be released under any other type of license. The
requirement for fonts to remain under this license does not apply
to any document created using the fonts or their derivatives.

DEFINITIONS
"Font Software" refers to the set of files released by the Copyright
Holder(s) under this license and clearly marked as such. This may
include source files, build scripts and documentation.

"Reserved Font Name" refers to any names specified as such after the
copyright statement(s).

"Original Version" refers to the collection of Font Software components as
distributed by the Copyright Holder(s).

"Modified Version" refers to any derivative made by adding to, deleting,
or substituting -- in part or in whole -- any of the components of the
Original Version, by changing formats or by porting the Font Software to a
new environment.

"Author" refers to any designer, engineer, programmer, technical
writer or other person who contributed to the Font Software.

PERMISSION & CONDITIONS
Permission is hereby granted, free of charge, to any person obtaining
a copy of the Font Software, to use, study, copy, merge, embed, modify,
redistribute, and sell modified and unmodified copies of the Font
Software, subject to the following conditions:

1) Neither the Font Software nor any of its individual components,
in Original or Modified Versions, may be sold by itself.

2) Original or Modified Versions of the Font Software may be bundled,
redistributed and/or sold with any software, provided that each copy
contains the above copyright notice and this license. These can be
included either as stand-alone text files, human-readable headers or
in the appropriate machine-readable metadata fields within text or
binary files as long as those fields can be easily viewed by the user.

3) No Modified Version of the Font Software may use the Reserved Font
Name(s) unless explicit written permission is granted by the corresponding
Copyright Holder. This restriction only applies to the primary font name as
presented to the users.

4) The name(s) of the Copyright Holder(s) or the Author(s) of the Font
Software shall not be used to promote, endorse or advertise any
Modified Version, except to acknowledge the contribution(s) of the
Copyright Holder(s) and the Author(s) or with their explicit written
permission.

5) The Font Software, modified or unmodified, in part or in whole,
must be distributed entirely under this license, and must not be
distributed under any other license. The requirement for fonts to
remain under this license does not apply to any document created
using the Font Software.

TERMINATION
This license becomes null and void if any of the above conditions are
not met.

DISCLAIMER
THE FONT SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO ANY WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT
OF COPYRIGHT, PATENT, TRADEMARK, OR OTHER RIGHT. IN NO EVENT SHALL THE
COPYRIGHT HOLDER BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
INCLUDING ANY GENERAL, SPECIAL, INDIRECT, INCIDENTAL, OR CONSEQUENTIAL
DAMAGES, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF THE USE OR INABILITY TO USE THE FONT SOFTWARE OR FROM
OTHER DEALINGS IN THE FONT SOFTWARE.
```

### JetBrains Mono (variable) - SIL Open Font License 1.1

Shipped via `@fontsource-variable/jetbrains-mono` (latin and latin-ext subsets).

```
Copyright 2020 The JetBrains Mono Project Authors (https://github.com/JetBrains/JetBrainsMono) JetBrainsMono-Italic[wght].ttf: Copyright 2020 The JetBrains Mono Project Authors (https://github.com/JetBrains/JetBrainsMono)

This Font Software is licensed under the SIL Open Font License, Version 1.1.
This license is copied below, and is also available with a FAQ at:
http://scripts.sil.org/OFL


-----------------------------------------------------------
SIL OPEN FONT LICENSE Version 1.1 - 26 February 2007
-----------------------------------------------------------

PREAMBLE
The goals of the Open Font License (OFL) are to stimulate worldwide
development of collaborative font projects, to support the font creation
efforts of academic and linguistic communities, and to provide a free and
open framework in which fonts may be shared and improved in partnership
with others.

The OFL allows the licensed fonts to be used, studied, modified and
redistributed freely as long as they are not sold by themselves. The
fonts, including any derivative works, can be bundled, embedded,
redistributed and/or sold with any software provided that any reserved
names are not used by derivative works. The fonts and derivatives,
however, cannot be released under any other type of license. The
requirement for fonts to remain under this license does not apply
to any document created using the fonts or their derivatives.

DEFINITIONS
"Font Software" refers to the set of files released by the Copyright
Holder(s) under this license and clearly marked as such. This may
include source files, build scripts and documentation.

"Reserved Font Name" refers to any names specified as such after the
copyright statement(s).

"Original Version" refers to the collection of Font Software components as
distributed by the Copyright Holder(s).

"Modified Version" refers to any derivative made by adding to, deleting,
or substituting -- in part or in whole -- any of the components of the
Original Version, by changing formats or by porting the Font Software to a
new environment.

"Author" refers to any designer, engineer, programmer, technical
writer or other person who contributed to the Font Software.

PERMISSION & CONDITIONS
Permission is hereby granted, free of charge, to any person obtaining
a copy of the Font Software, to use, study, copy, merge, embed, modify,
redistribute, and sell modified and unmodified copies of the Font
Software, subject to the following conditions:

1) Neither the Font Software nor any of its individual components,
in Original or Modified Versions, may be sold by itself.

2) Original or Modified Versions of the Font Software may be bundled,
redistributed and/or sold with any software, provided that each copy
contains the above copyright notice and this license. These can be
included either as stand-alone text files, human-readable headers or
in the appropriate machine-readable metadata fields within text or
binary files as long as those fields can be easily viewed by the user.

3) No Modified Version of the Font Software may use the Reserved Font
Name(s) unless explicit written permission is granted by the corresponding
Copyright Holder. This restriction only applies to the primary font name as
presented to the users.

4) The name(s) of the Copyright Holder(s) or the Author(s) of the Font
Software shall not be used to promote, endorse or advertise any
Modified Version, except to acknowledge the contribution(s) of the
Copyright Holder(s) and the Author(s) or with their explicit written
permission.

5) The Font Software, modified or unmodified, in part or in whole,
must be distributed entirely under this license, and must not be
distributed under any other license. The requirement for fonts to
remain under this license does not apply to any document created
using the Font Software.

TERMINATION
This license becomes null and void if any of the above conditions are
not met.

DISCLAIMER
THE FONT SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO ANY WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT
OF COPYRIGHT, PATENT, TRADEMARK, OR OTHER RIGHT. IN NO EVENT SHALL THE
COPYRIGHT HOLDER BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
INCLUDING ANY GENERAL, SPECIAL, INDIRECT, INCIDENTAL, OR CONSEQUENTIAL
DAMAGES, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF THE USE OR INABILITY TO USE THE FONT SOFTWARE OR FROM
OTHER DEALINGS IN THE FONT SOFTWARE.
```

### Lucide icons - ISC License

The glyphs in `src/lib/icons.ts` are a subset of [Lucide](https://lucide.dev).

```
ISC License

Copyright (c) 2026 Lucide Icons and Contributors

Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

---

The following Lucide icons are derived from the Feather project:

airplay, alert-circle, alert-octagon, alert-triangle, aperture, arrow-down-circle, arrow-down-left, arrow-down-right, arrow-down, arrow-left-circle, arrow-left, arrow-right-circle, arrow-right, arrow-up-circle, arrow-up-left, arrow-up-right, arrow-up, at-sign, calendar, cast, check, chevron-down, chevron-left, chevron-right, chevron-up, chevrons-down, chevrons-left, chevrons-right, chevrons-up, circle, clipboard, clock, code, columns, command, compass, corner-down-left, corner-down-right, corner-left-down, corner-left-up, corner-right-down, corner-right-up, corner-up-left, corner-up-right, crosshair, database, divide-circle, divide-square, dollar-sign, download, external-link, feather, frown, hash, headphones, help-circle, info, italic, key, layout, life-buoy, link-2, link, loader, lock, log-in, log-out, maximize, meh, minimize, minimize-2, minus-circle, minus-square, minus, monitor, moon, more-horizontal, more-vertical, move, music, navigation-2, navigation, octagon, pause-circle, percent, plus-circle, plus-square, plus, power, radio, rss, search, server, share, shopping-bag, sidebar, smartphone, smile, square, table-2, tablet, target, terminal, trash-2, trash, triangle, tv, type, upload, x-circle, x-octagon, x-square, x, zoom-in, zoom-out

The MIT License (MIT) (for the icons listed above)

Copyright (c) 2013-present Cole Bemis

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Bundled binaries and libraries

### ffmpeg — LGPL v3 or later

`ffmpeg.exe` and `ffprobe.exe` are installed beside the application, in its `binaries/`
directory, and are what lets an installed application clip on a machine that has no system
ffmpeg at all. Neither binary is committed: `cargo xtask sidecars fetch` downloads the
pinned archive, checks it against the SHA-256 recorded in `xtask/sidecars.toml`, and
extracts only the two allowlisted paths out of it.

| | |
|---|---|
| Upstream project | FFmpeg — <https://ffmpeg.org> |
| Build producer | BtbN/FFmpeg-Builds — <https://github.com/BtbN/FFmpeg-Builds> |
| Variant | the `win64` / `lgpl` build: the LGPL set of libraries, without the GPL-only ones (libx264, libx265), built from the FFmpeg 9.0 release branch |
| Licence | LGPL v3 or later (upstream FFmpeg itself is LGPL v2.1 or later) |

The exact archive and its SHA-256 are pinned in `xtask/sidecars.toml`; the archive's own
name carries the FFmpeg version it was built from.

#### The source offer

The LGPL asks that the source of the libraries these binaries contain be made available
alongside them. For this build that is:

- the FFmpeg source of the release branch the archive's name names, from
  <https://ffmpeg.org/download.html> and <https://git.ffmpeg.org/ffmpeg.git>; and
- the build configuration that produced these exact binaries — the producer's `win64` /
  `lgpl` recipe and the dependency set it enables — from
  <https://github.com/BtbN/FFmpeg-Builds> (`build.sh` and `scripts.d/`).

### The Tauri runtime and its plugins — Apache-2.0 OR MIT

The window is a Tauri v2 application: its Rust side is compiled into `localplay.exe`, and
the webview it drives is the platform's own (WebView2 on Windows) — no browser engine is
shipped.

| Crate | Version | Licence |
|---|---|---|
| `tauri` | 2.12.4 | Apache-2.0 OR MIT |
| `tauri-runtime` | 2.12.2 | Apache-2.0 OR MIT |
| `tauri-runtime-wry` | 2.12.2 | Apache-2.0 OR MIT |
| `tauri-utils` | 2.10.2 | Apache-2.0 OR MIT |
| `tauri-build` | 2.7.3 | Apache-2.0 OR MIT |
| `tauri-codegen` | 2.7.3 | Apache-2.0 OR MIT |
| `tauri-macros` | 2.7.3 | Apache-2.0 OR MIT |
| `tauri-plugin` | 2.7.2 | Apache-2.0 OR MIT |
| `wry` | 0.57.0 | Apache-2.0 OR MIT |
| `tao` | 0.37.1 | Apache-2.0 |
| `tauri-plugin-dialog` | 2.8.3 | Apache-2.0 OR MIT |
| `tauri-plugin-single-instance` | 2.5.4 | Apache-2.0 OR MIT |
| `tauri-plugin-notification` | 2.7.0 | Apache-2.0 OR MIT |

`tauri-plugin-fs` (2.7.1) comes in with `tauri-plugin-dialog` and is listed with
everything else below. Upstream: <https://github.com/tauri-apps/tauri> and
<https://github.com/tauri-apps/plugins-workspace>. The full text of the MIT and
Apache-2.0 licences is in this repository as `LICENSE-MIT` and `LICENSE-APACHE`.

### The npm packages compiled into the window — Apache-2.0 OR MIT

Two npm packages are compiled into `dist/`, and are therefore part of the shipped
application: the Tauri IPC API the frontend calls, and the frontend half of the dialog
plugin. (`@tauri-apps/cli` is a build-time tool: it drives the bundler on the machine
that builds the application and is not part of it.)

| Package | Version | Licence | Upstream |
|---|---|---|---|
| `@tauri-apps/api` | 2.12.4 | Apache-2.0 OR MIT | <https://github.com/tauri-apps/tauri> |
| `@tauri-apps/plugin-dialog` | 2.8.2 | MIT OR Apache-2.0 | <https://github.com/tauri-apps/plugins-workspace> |

### The Rust crates compiled into the application

284 third-party crates are compiled into `localplay.exe` for the Windows target. Each
licence is named below exactly as the crate declares it. The full text of each licence is
in that licence's own upstream project — `LICENSE-MIT` and `LICENSE-APACHE` in this
repository for the two that cover most of the list, and the upstream project for the
rest.

The list is the resolved dependency graph of the shipped target, and it is regenerated
with:

```console
$ cargo tree --manifest-path apps/desktop/src-tauri/Cargo.toml \
    --target x86_64-pc-windows-msvc --edges normal,build --prefix none
$ cargo metadata --manifest-path apps/desktop/src-tauri/Cargo.toml --format-version 1
```

| Crate | Version | Licence |
|---|---|---|
| `adler2` | 2.0.1 | 0BSD OR MIT OR Apache-2.0 |
| `aho-corasick` | 1.1.5 | Unlicense OR MIT |
| `alloc-no-stdlib` | 3.0.0 | BSD-3-Clause |
| `alloc-stdlib` | 0.3.0 | BSD-3-Clause |
| `anyhow` | 1.0.104 | MIT OR Apache-2.0 |
| `autocfg` | 1.5.1 | Apache-2.0 OR MIT |
| `base64` | 0.21.7 | MIT OR Apache-2.0 |
| `base64` | 0.23.1 | MIT OR Apache-2.0 |
| `bit-set` | 0.8.0 | Apache-2.0 OR MIT |
| `bit-vec` | 0.8.0 | Apache-2.0 OR MIT |
| `bitflags` | 1.3.2 | MIT/Apache-2.0 |
| `bitflags` | 2.13.2 | MIT OR Apache-2.0 |
| `block-buffer` | 0.10.4 | MIT OR Apache-2.0 |
| `brotli` | 9.0.0 | BSD-3-Clause AND MIT |
| `brotli-decompressor` | 6.0.1 | BSD-3-Clause/MIT |
| `bytemuck` | 1.25.2 | Zlib OR Apache-2.0 OR MIT |
| `byteorder` | 1.5.0 | Unlicense OR MIT |
| `byteorder-lite` | 0.1.0 | Unlicense OR MIT |
| `bytes` | 1.12.1 | MIT |
| `camino` | 1.2.6 | MIT OR Apache-2.0 |
| `cargo-platform` | 0.1.9 | MIT OR Apache-2.0 |
| `cargo_metadata` | 0.19.2 | MIT |
| `cargo_toml` | 1.0.1 | Apache-2.0 OR MIT |
| `cc` | 1.8.0 | MIT OR Apache-2.0 |
| `cfb` | 0.14.0 | MIT |
| `cfg-if` | 1.0.5 | MIT OR Apache-2.0 |
| `cookie` | 0.18.2 | MIT OR Apache-2.0 |
| `cpufeatures` | 0.2.17 | MIT OR Apache-2.0 |
| `crc32fast` | 1.5.2 | MIT OR Apache-2.0 |
| `crossbeam-channel` | 0.5.17 | MIT OR Apache-2.0 |
| `crossbeam-utils` | 0.8.23 | MIT OR Apache-2.0 |
| `crypto-common` | 0.1.7 | MIT OR Apache-2.0 |
| `cssparser` | 0.37.0 | MPL-2.0 |
| `cssparser-macros` | 0.7.1 | MPL-2.0 |
| `ctor` | 1.0.13 | Apache-2.0 OR MIT |
| `darling` | 0.24.1 | MIT |
| `darling_core` | 0.24.1 | MIT |
| `darling_macro` | 0.24.1 | MIT |
| `deranged` | 0.5.9 | MIT OR Apache-2.0 |
| `derive_more` | 2.1.1 | MIT |
| `derive_more-impl` | 2.1.1 | MIT |
| `digest` | 0.10.7 | MIT OR Apache-2.0 |
| `dirs` | 7.0.0 | MIT OR Apache-2.0 |
| `dirs-sys` | 0.5.0 | MIT OR Apache-2.0 |
| `displaydoc` | 0.2.7 | MIT OR Apache-2.0 |
| `dom_query` | 0.28.0 | MIT |
| `dpi` | 0.1.2 | Apache-2.0 AND MIT |
| `drag` | 2.1.1 | Apache-2.0 OR MIT |
| `dtoa` | 1.0.11 | MIT OR Apache-2.0 |
| `dtoa-short` | 0.3.5 | MPL-2.0 |
| `dunce` | 1.0.5 | CC0-1.0 OR MIT-0 OR Apache-2.0 |
| `dyn-clone` | 1.0.20 | MIT OR Apache-2.0 |
| `embed-resource` | 3.0.12 | MIT |
| `equivalent` | 1.0.3 | Apache-2.0 OR MIT |
| `erased-serde` | 0.4.10 | MIT OR Apache-2.0 |
| `fallible-iterator` | 0.3.0 | MIT/Apache-2.0 |
| `fallible-streaming-iterator` | 0.1.9 | MIT/Apache-2.0 |
| `fastrand` | 2.5.0 | Apache-2.0 OR MIT |
| `fdeflate` | 0.3.7 | MIT OR Apache-2.0 |
| `find-msvc-tools` | 0.1.14 | MIT OR Apache-2.0 |
| `flate2` | 1.1.10 | MIT OR Apache-2.0 |
| `fnv` | 1.0.7 | Apache-2.0 / MIT |
| `foldhash` | 0.2.0 | Zlib |
| `form_urlencoded` | 1.2.2 | MIT OR Apache-2.0 |
| `fs4` | 0.13.1 | MIT OR Apache-2.0 |
| `generic-array` | 0.14.7 | MIT |
| `getrandom` | 0.3.4 | MIT OR Apache-2.0 |
| `getrandom` | 0.4.3 | MIT OR Apache-2.0 |
| `glob` | 0.3.4 | MIT OR Apache-2.0 |
| `hashbrown` | 0.12.3 | MIT OR Apache-2.0 |
| `hashbrown` | 0.17.1 | MIT OR Apache-2.0 |
| `hashlink` | 0.12.2 | MIT OR Apache-2.0 |
| `heck` | 0.5.0 | MIT OR Apache-2.0 |
| `html5ever` | 0.39.0 | MIT OR Apache-2.0 |
| `http` | 1.5.0 | MIT OR Apache-2.0 |
| `http-range` | 0.1.5 | MIT |
| `ico` | 0.5.0 | MIT |
| `icu_collections` | 2.3.0 | Unicode-3.0 |
| `icu_locale_core` | 2.3.0 | Unicode-3.0 |
| `icu_normalizer` | 2.3.0 | Unicode-3.0 |
| `icu_normalizer_data` | 2.3.0 | Unicode-3.0 |
| `icu_properties` | 2.3.0 | Unicode-3.0 |
| `icu_properties_data` | 2.3.0 | Unicode-3.0 |
| `icu_provider` | 2.3.1 | Unicode-3.0 |
| `ident_case` | 1.0.1 | MIT/Apache-2.0 |
| `idna` | 1.1.0 | MIT OR Apache-2.0 |
| `idna_adapter` | 1.2.2 | Apache-2.0 OR MIT |
| `image` | 0.25.10 | MIT OR Apache-2.0 |
| `indexmap` | 1.9.3 | Apache-2.0 OR MIT |
| `indexmap` | 2.14.2 | Apache-2.0 OR MIT |
| `infer` | 0.22.0 | MIT |
| `itoa` | 1.0.18 | MIT OR Apache-2.0 |
| `json-patch` | 4.2.0 | MIT/Apache-2.0 |
| `jsonptr` | 0.7.1 | MIT OR Apache-2.0 |
| `keyboard-types` | 0.8.3 | MIT OR Apache-2.0 |
| `lazy_static` | 1.5.1 | MIT OR Apache-2.0 |
| `libc` | 0.2.190 | MIT OR Apache-2.0 |
| `libsqlite3-sys` | 0.38.2 | MIT |
| `litemap` | 0.8.3 | Unicode-3.0 |
| `lock_api` | 0.4.14 | MIT OR Apache-2.0 |
| `log` | 0.4.34 | MIT OR Apache-2.0 |
| `markup5ever` | 0.39.0 | MIT OR Apache-2.0 |
| `matchers` | 0.2.0 | MIT |
| `memchr` | 2.8.3 | Unlicense OR MIT |
| `mime` | 0.3.17 | MIT OR Apache-2.0 |
| `miniz_oxide` | 0.8.9 | MIT OR Zlib OR Apache-2.0 |
| `miniz_oxide` | 0.9.1 | MIT OR Zlib OR Apache-2.0 |
| `mio` | 1.2.4 | MIT |
| `moxcms` | 0.8.1 | BSD-3-Clause OR Apache-2.0 |
| `muda` | 0.20.0 | Apache-2.0 OR MIT |
| `native-tls` | 0.2.18 | MIT OR Apache-2.0 |
| `new_debug_unreachable` | 1.0.6 | MIT |
| `notify-rust` | 4.18.2 | MIT OR Apache-2.0 |
| `nu-ansi-term` | 0.50.3 | MIT |
| `num-conv` | 0.2.2 | MIT OR Apache-2.0 |
| `num-traits` | 0.2.19 | MIT OR Apache-2.0 |
| `objc2` | 0.6.5 | MIT |
| `objc2-encode` | 4.1.0 | MIT |
| `objc2-foundation` | 0.3.2 | MIT |
| `once_cell` | 1.21.4 | MIT OR Apache-2.0 |
| `option-ext` | 0.2.0 | MPL-2.0 |
| `parking_lot` | 0.12.5 | MIT OR Apache-2.0 |
| `parking_lot_core` | 0.9.12 | MIT OR Apache-2.0 |
| `percent-encoding` | 2.3.2 | MIT OR Apache-2.0 |
| `phf` | 0.13.1 | MIT |
| `phf_codegen` | 0.13.1 | MIT |
| `phf_generator` | 0.13.1 | MIT |
| `phf_macros` | 0.13.1 | MIT |
| `phf_shared` | 0.13.1 | MIT |
| `pin-project-lite` | 0.2.17 | Apache-2.0 OR MIT |
| `pkg-config` | 0.3.34 | MIT OR Apache-2.0 |
| `plist` | 1.10.1 | MIT |
| `png` | 0.17.16 | MIT OR Apache-2.0 |
| `png` | 0.18.1 | MIT OR Apache-2.0 |
| `potential_utf` | 0.1.6 | Unicode-3.0 |
| `powerfmt` | 0.2.1 | MIT OR Apache-2.0 |
| `ppv-lite86` | 0.2.21 | MIT OR Apache-2.0 |
| `precomputed-hash` | 0.1.1 | MIT |
| `proc-macro2` | 1.0.107 | MIT OR Apache-2.0 |
| `pxfm` | 0.1.30 | BSD-3-Clause OR Apache-2.0 |
| `quick-xml` | 0.42.0 | MIT |
| `quote` | 1.0.47 | MIT OR Apache-2.0 |
| `rand` | 0.9.5 | MIT OR Apache-2.0 |
| `rand_chacha` | 0.9.0 | MIT OR Apache-2.0 |
| `rand_core` | 0.9.5 | MIT OR Apache-2.0 |
| `raw-window-handle` | 0.6.2 | MIT OR Apache-2.0 OR Zlib |
| `regex` | 1.13.1 | MIT OR Apache-2.0 |
| `regex-automata` | 0.4.18 | MIT OR Apache-2.0 |
| `regex-syntax` | 0.8.11 | MIT OR Apache-2.0 |
| `rfd` | 0.16.0 | MIT |
| `rusqlite` | 0.40.2 | MIT |
| `rustc-hash` | 2.1.3 | Apache-2.0 OR MIT |
| `rustc_version` | 0.4.1 | MIT OR Apache-2.0 |
| `same-file` | 1.0.6 | Unlicense/MIT |
| `schannel` | 0.1.30 | MIT |
| `schemars` | 0.8.22 | MIT |
| `schemars_derive` | 0.8.22 | MIT |
| `scopeguard` | 1.2.0 | MIT OR Apache-2.0 |
| `selectors` | 0.38.0 | MPL-2.0 |
| `semver` | 1.0.28 | MIT OR Apache-2.0 |
| `serde` | 1.0.229 | MIT OR Apache-2.0 |
| `serde-untagged` | 0.1.9 | MIT OR Apache-2.0 |
| `serde_core` | 1.0.229 | MIT OR Apache-2.0 |
| `serde_derive` | 1.0.229 | MIT OR Apache-2.0 |
| `serde_derive_internals` | 0.29.1 | MIT OR Apache-2.0 |
| `serde_json` | 1.0.154 | MIT OR Apache-2.0 |
| `serde_repr` | 0.1.21 | MIT OR Apache-2.0 |
| `serde_spanned` | 1.1.2 | MIT OR Apache-2.0 |
| `serde_with` | 3.24.0 | MIT OR Apache-2.0 |
| `serde_with_macros` | 3.24.0 | MIT OR Apache-2.0 |
| `serialize-to-javascript` | 0.1.2 | MIT OR Apache-2.0 |
| `serialize-to-javascript-impl` | 0.1.2 | MIT OR Apache-2.0 |
| `servo_arc` | 0.4.3 | MIT OR Apache-2.0 |
| `sha2` | 0.10.9 | MIT OR Apache-2.0 |
| `sharded-slab` | 0.1.7 | MIT |
| `shlex` | 2.0.1 | MIT OR Apache-2.0 |
| `simd-adler32` | 0.3.10 | MIT |
| `siphasher` | 1.0.4 | MIT OR Apache-2.0 |
| `smallvec` | 1.16.3 | MIT OR Apache-2.0 |
| `socket2` | 0.6.5 | MIT OR Apache-2.0 |
| `softbuffer` | 0.4.8 | MIT OR Apache-2.0 |
| `stable_deref_trait` | 1.2.1 | MIT OR Apache-2.0 |
| `static_vcruntime` | 3.0.0 | MIT OR Apache-2.0 OR Zlib |
| `string_cache` | 0.9.0 | MIT OR Apache-2.0 |
| `string_cache_codegen` | 0.6.1 | MIT OR Apache-2.0 |
| `strsim` | 0.11.1 | MIT |
| `subtle` | 2.6.1 | BSD-3-Clause |
| `swift-rs` | 1.0.9 | MIT OR Apache-2.0 |
| `syn` | 2.0.119 | MIT OR Apache-2.0 |
| `syn` | 3.0.9 | MIT OR Apache-2.0 |
| `synstructure` | 0.14.0 | MIT |
| `tao` | 0.37.1 | Apache-2.0 |
| `tauri` | 2.12.4 | Apache-2.0 OR MIT |
| `tauri-build` | 2.7.3 | Apache-2.0 OR MIT |
| `tauri-codegen` | 2.7.3 | Apache-2.0 OR MIT |
| `tauri-macros` | 2.7.3 | Apache-2.0 OR MIT |
| `tauri-plugin` | 2.7.2 | Apache-2.0 OR MIT |
| `tauri-plugin-dialog` | 2.8.3 | Apache-2.0 OR MIT |
| `tauri-plugin-fs` | 2.7.1 | Apache-2.0 OR MIT |
| `tauri-plugin-notification` | 2.7.0 | Apache-2.0 OR MIT |
| `tauri-plugin-single-instance` | 2.5.4 | Apache-2.0 OR MIT |
| `tauri-runtime` | 2.12.2 | Apache-2.0 OR MIT |
| `tauri-runtime-wry` | 2.12.2 | Apache-2.0 OR MIT |
| `tauri-utils` | 2.10.2 | Apache-2.0 OR MIT |
| `tauri-winres` | 0.3.6 | MIT |
| `tauri-winrt-notification` | 0.8.1 | MIT OR Apache-2.0 |
| `tendril` | 0.5.1 | MIT OR Apache-2.0 |
| `thiserror` | 2.0.21 | MIT OR Apache-2.0 |
| `thiserror-impl` | 2.0.21 | MIT OR Apache-2.0 |
| `thread_local` | 1.1.10 | MIT OR Apache-2.0 |
| `time` | 0.3.55 | MIT OR Apache-2.0 |
| `time-core` | 0.1.9 | MIT OR Apache-2.0 |
| `time-macros` | 0.2.32 | MIT OR Apache-2.0 |
| `tinystr` | 0.8.4 | Unicode-3.0 |
| `tokio` | 1.53.3 | MIT |
| `toml` | 1.1.8+spec-1.1.0 | MIT OR Apache-2.0 |
| `toml_datetime` | 1.1.2+spec-1.1.0 | MIT OR Apache-2.0 |
| `toml_edit` | 0.25.17+spec-1.1.0 | MIT OR Apache-2.0 |
| `toml_parser` | 1.1.5+spec-1.1.0 | MIT OR Apache-2.0 |
| `toml_writer` | 1.1.3+spec-1.1.0 | MIT OR Apache-2.0 |
| `tracing` | 0.1.44 | MIT |
| `tracing-attributes` | 0.1.31 | MIT |
| `tracing-core` | 0.1.36 | MIT |
| `tracing-log` | 0.2.0 | MIT |
| `tracing-subscriber` | 0.3.23 | MIT |
| `tray-icon` | 0.25.1 | MIT OR Apache-2.0 |
| `typeid` | 1.0.3 | MIT OR Apache-2.0 |
| `typenum` | 1.20.1 | MIT OR Apache-2.0 |
| `unicode-ident` | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| `unicode-segmentation` | 1.13.3 | MIT OR Apache-2.0 |
| `url` | 2.5.8 | MIT OR Apache-2.0 |
| `urlpattern` | 0.6.0 | MIT |
| `utf8_iter` | 1.0.4 | Apache-2.0 OR MIT |
| `uuid` | 1.28.0 | Apache-2.0 OR MIT |
| `vcpkg` | 0.2.15 | MIT/Apache-2.0 |
| `version_check` | 0.9.5 | MIT/Apache-2.0 |
| `walkdir` | 2.5.0 | Unlicense/MIT |
| `web-time` | 1.1.0 | MIT OR Apache-2.0 |
| `web_atoms` | 0.2.6 | MIT OR Apache-2.0 |
| `webview2-com` | 0.39.1 | MIT |
| `webview2-com-macros` | 0.8.1 | MIT |
| `webview2-com-sys` | 0.39.1 | MIT |
| `winapi-util` | 0.1.11 | Unlicense OR MIT |
| `window-vibrancy` | 0.8.1 | Apache-2.0 OR MIT |
| `windows` | 0.52.0 | MIT OR Apache-2.0 |
| `windows` | 0.62.2 | MIT OR Apache-2.0 |
| `windows-collections` | 0.3.2 | MIT OR Apache-2.0 |
| `windows-core` | 0.52.0 | MIT OR Apache-2.0 |
| `windows-core` | 0.58.0 | MIT OR Apache-2.0 |
| `windows-core` | 0.62.2 | MIT OR Apache-2.0 |
| `windows-future` | 0.3.2 | MIT OR Apache-2.0 |
| `windows-implement` | 0.52.0 | MIT OR Apache-2.0 |
| `windows-implement` | 0.58.0 | MIT OR Apache-2.0 |
| `windows-implement` | 0.60.2 | MIT OR Apache-2.0 |
| `windows-interface` | 0.52.0 | MIT OR Apache-2.0 |
| `windows-interface` | 0.58.0 | MIT OR Apache-2.0 |
| `windows-interface` | 0.59.3 | MIT OR Apache-2.0 |
| `windows-link` | 0.2.1 | MIT OR Apache-2.0 |
| `windows-numerics` | 0.3.1 | MIT OR Apache-2.0 |
| `windows-result` | 0.2.0 | MIT OR Apache-2.0 |
| `windows-result` | 0.4.1 | MIT OR Apache-2.0 |
| `windows-strings` | 0.1.0 | MIT OR Apache-2.0 |
| `windows-strings` | 0.5.1 | MIT OR Apache-2.0 |
| `windows-sys` | 0.59.0 | MIT OR Apache-2.0 |
| `windows-sys` | 0.60.2 | MIT OR Apache-2.0 |
| `windows-sys` | 0.61.2 | MIT OR Apache-2.0 |
| `windows-targets` | 0.52.6 | MIT OR Apache-2.0 |
| `windows-targets` | 0.53.5 | MIT OR Apache-2.0 |
| `windows-threading` | 0.2.1 | MIT OR Apache-2.0 |
| `windows-version` | 0.1.7 | MIT OR Apache-2.0 |
| `windows_x86_64_msvc` | 0.52.6 | MIT OR Apache-2.0 |
| `windows_x86_64_msvc` | 0.53.1 | MIT OR Apache-2.0 |
| `winnow` | 1.0.4 | MIT |
| `writeable` | 0.6.4 | Unicode-3.0 |
| `wry` | 0.57.0 | Apache-2.0 OR MIT |
| `yoke` | 0.8.3 | Unicode-3.0 |
| `yoke-derive` | 0.8.4 | Unicode-3.0 |
| `zerocopy` | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT |
| `zerofrom` | 0.1.8 | Unicode-3.0 |
| `zerofrom-derive` | 0.1.8 | Unicode-3.0 |
| `zerotrie` | 0.2.5 | Unicode-3.0 |
| `zerovec` | 0.11.8 | Unicode-3.0 |
| `zerovec-derive` | 0.11.6 | Unicode-3.0 |
| `zmij` | 1.0.23 | MIT |
