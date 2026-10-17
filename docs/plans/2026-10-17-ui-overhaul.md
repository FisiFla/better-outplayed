# Desktop UI overhaul — plan

Status: **plan only**, nothing implemented. Scope: `apps/desktop/src/**` (Svelte 5 frontend),
`apps/desktop/index.html`, `apps/desktop/scripts/shots.mjs`, and the window background colour in
`src-tauri/tauri.conf.json`. No Rust, no IPC, no crate changes.

## 0. Why

The window feels old and odd because of its shape, not its features:

- One 320–380px sidebar (`.sidebar` in `App.svelte`) stacks Recorder, Clips, Sessions, Storage,
  Health and Settings in a single scroll column; the 900px main pane is only a detail view and is
  an empty sentence until something is picked.
- Clips are 1-line rows with a postage-stamp thumbnail — in a video app the thumbnail *is* the
  content. Thumbnails are already full-resolution JPEGs (`media::edit::thumbnail`, `-q:v 4`), so
  big cards cost no backend work.
- Flat theme (`app.css`: `#0b0c10` / `#14161c`, teal `#5eead4`), `system-ui` font, 13px
  uppercase-letterspaced `h2` labels everywhere, no icons (the star is a `★` glyph, actions are
  text buttons), no navigation, no motion, three `<details>` disclosures hiding Storage, Health
  and Settings.

## 1. Goals, non-goals, hard constraints

**Goals**
1. A navigable app: a left icon rail with five destinations; each view owns the full main area.
2. Library = a grid of large thumbnail cards; detail is a dedicated page with a big player.
3. Recorder state is always visible (persistent top bar), never scrolled away.
4. A real design system: semantic tokens, a bundled typeface, a bundled icon set, elevation,
   motion, consistent focus rings.
5. Every existing capability stays reachable in ≤ 2 clicks from any view.

**Non-goals**
- No new features, commands or settings. No change to what a button *does*.
- No router library, no component library (shadcn/skeleton/etc.), no CSS framework, no state lib.
- No re-design of the trim/scrub interaction model (`Timeline.svelte` keeps its geometry and
  pointer→ms mapping; only its skin changes).

**Hard constraints (each phase's acceptance re-checks all of them)**
- **Zero network (principle 1).** No CDN fonts, no remote icons, no `@import url(http…)`. Fonts
  and icons ship inside `dist/`. The CSP (`font-src 'self' data:`, `img-src 'self' asset: …`)
  already forbids anything else and must **not** be loosened. Add a build check:
  `grep -rE "https?://" dist/ --include='*.css'` must find nothing (fold into `npm run check` or
  a vitest that reads `dist` — decided in Phase 0).
- **IPC untouched.** `src/lib/ipc.ts`, `src/lib/types.ts` and the pure helpers
  (`clips.ts`, `sessions.ts`, `recording.ts`, `health.ts`, `settings.ts`, `trim.ts`, `markers.ts`,
  `matches.ts`, `time.ts`) are read-only. New logic goes in new files with tests (e.g.
  `src/lib/nav.ts` + `nav.test.ts`). Every `source.*` call in `App.svelte` keeps its arguments.
- **The `ClipSource` seam.** `App` keeps its `source` prop and `main.ts` its test seam; the a11y
  test and `shots.mjs` both depend on them.
- **Accessibility.** `src/a11y.test.ts` (axe, jsdom) must stay at zero violations and gets a case
  per view. `shots.mjs`'s `auditContrast` (WCAG 2.1 AA, 4.5:1 normal / 3:1 large) must pass for
  every label in `CONTRAST_LABELS`; labels are updated with the selectors, never deleted to make
  a failure go away. Focus is always visible; nothing is reachable only by hover or drag.
- **Existing tests.** `npm run check`, `npm test` (10 `lib/*.test.ts` + a11y), and `npm run shots`
  green at the end of every phase.
- **Window.** Default 1280×880, min **940×600** (`tauri.conf.json`). shots renders at 1280×800 and
  has a short-window state (`15-short-window`). Every view must work at 940×600 with no
  horizontal overflow (an existing `layoutAudit` assertion).
- **Dark from the first frame.** `index.html` inline style, `tauri.conf.json` `backgroundColor`
  and `--bg` must be the same hex.

## 2. Information architecture

```
Rail (64px)          Top bar (52px, always visible)
 ┌──┐ ┌────────────────────────────────────────────────────────────────────┐
 │▶ │ │ ● REC  Buffer · 1:42 of 2:00 · 60 fps     [Save clip ⌃⇧C] [Stop] ▾ │
 │▤ │ ├────────────────────────────────────────────────────────────────────┤
 │  │ │                                                                    │
 │  │ │                      active view                                   │
 │⛁ │ │                                                                    │
 │♥•│ │                                                                    │
 │⚙ │ │                                                         toasts ▪▪ │
 └──┘ └────────────────────────────────────────────────────────────────────┘
```

Rail destinations (top group / bottom group), each a `<button aria-current="page">` with an icon,
a visible 10px label under it, and a tooltip:

| # | View | Icon (Lucide) | Contents (from today's component) |
|---|------|---------------|-----------------------------------|
| 1 | **Library** (default) | `clapperboard` | `ClipFilterBar` + clip grid (`ClipList`) → Clip page (`ClipDetail`) |
| 2 | **Sessions** | `list-video` | `SessionList` → Session page (`SessionDetail` + `MatchTimeline`) |
| 3 | **Storage** | `hard-drive` | `StoragePanel`, full width, no disclosure |
| 4 | **Health** | `activity` | `HealthPanel`, full width; rail shows a dot badge when unhealthy |
| 5 | **Settings** | `settings` | `SettingsPanel`, sections as a sub-nav, no disclosure |

Navigation state is a single `$state<View>` in `App.svelte` — no URL router:
`{ kind: 'library' } | { kind: 'clip', id } | { kind: 'sessions' } | { kind: 'session', id } |
{ kind: 'storage' } | { kind: 'health' } | { kind: 'settings' }`. Pure transitions
(select, back, delete-the-open-item → back to list, rail badge from `HealthDto`) live in
`src/lib/nav.ts` with unit tests. `Esc` / `Alt+←` = back from a detail page. Selecting a clip no
longer has to "take the main pane back from a session" (today's comment in `App.svelte`): they are
different views.

### Wireframes (1280 wide; at 940 the grid drops to 3 columns, sessions list to 300px)

**Library**
```
┌ Library ──────────────────────────────── 12 of 48 clips ┐
│ [🔍 Search clips…        ] [Game ▾] [Tag ▾] [★ only] [Sort ▾]   │
│ ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────┐            │
│ │  thumb   │ │  thumb   │ │  thumb   │ │  thumb   │  16:9,     │
│ │ ★   0:30 │ │     1:12 │ │     0:45 │ │ ★   0:30 │  duration  │
│ └──────────┘ └──────────┘ └──────────┘ └──────────┘  badge     │
│ Ace on Bind   Clutch 1v3   Untitled     Baron steal            │
│ Valorant·2h   CS2·Yesterday …                                  │
│ #ace #rank                                    hover/focus: ⋯ menu│
│ …                                                               │
└─────────────────────────────────────────────────────────────────┘
Empty: large icon + "No clips yet — press Ctrl+Shift+C while recording." + [Start recording]
```
Card = one `<button>` (open) + an overlay action group (favourite toggle, `⋯` menu: Copy file,
Reveal, Delete). Drag-out stays on the card (pointer path unchanged). `auto-fill,
minmax(220px, 1fr)` grid; thumbnails `loading="lazy"`, `aspect-ratio: 16/9`, `object-fit: cover`.

**Clip page**
```
┌ ‹ Library   Ace on Bind ✎          [★] [Copy] [Reveal] [⋯ Delete] ┐
│ ┌───────────────────────────────────────────────┐ ┌ Details ─────┐ │
│ │                                               │ │ Valorant     │ │
│ │                 <video>                       │ │ 0:30 · 42 MB │ │
│ │                                               │ │ h264 · 1080p │ │
│ └───────────────────────────────────────────────┘ │ Tags [#ace×] │ │
│ ├──■────────◆──────◆───────────■─────┤  Timeline │ [+ tag]      │ │
│ 0:04.2 → 0:21.7 (17.5 s)       [Trim — lossless] └──────────────┘ │
└────────────────────────────────────────────────────────────────────┘
```
Side column collapses under the player below 1100px.

**Sessions** (master–detail, because a session's value is its marker list)
```
┌ Sessions ─────────── 6 ┐┌ Valorant · Oct 16, 21:04 · 41 min    [★][⋯] ┐
│ ● Valorant  41:12  ★   ││ ┌──────────── <video> ───────────────────┐  │
│   Oct 16 · 3.1 GB      ││ └────────────────────────────────────────┘  │
│ ○ CS2       12:40      ││ Matches ▬▬▬▬▬▬▬▬|▬▬▬▬▬▬▬▬|▬▬▬▬  (MatchTimeline)│
│   Oct 15 · 900 MB      ││ ├──◆──◆───◆────◆──────┤ markers (Timeline)  │
│ …                      ││ Kill 3:12 · Death 5:40 … [Extract clip]      │
└────────────────────────┘└──────────────────────────────────────────────┘
```
`Sessions` with nothing selected shows the most recent session (or an empty state), never a blank
pane. Clicking a session replaces the right side; there is no separate session page in this view
(so `{kind:'session'}` = sessions view with a selection).

**Storage**
```
┌ Storage ────────────────────────────────────────────────┐
│ ┌ Used ───────┐ ┌ Cap ───────┐ ┌ Favourites ┐ ┌ Missing ┐│
│ │ 31.2 GiB    │ │ 50 GiB     │ │ 4.1 GiB    │ │ 0 files ││
│ └─────────────┘ └────────────┘ └────────────┘ └─────────┘│
│ ▓▓▓▓▓▓▓▓▓▓▓▓▓▓░░░░░░  62% of cap   ✓ Cap met            │
│ Policy: 30 days · Next sweep deletes 0 clips             │
│ Clips folder  C:\…\clips                   [Reveal]      │
│ Warnings (verdict.bad rows, unchanged copy)              │
└──────────────────────────────────────────────────────────┘
```

**Health** — status list with an icon per row (ok / warn / error): recorder, encoder, ffmpeg,
disk, each event source, hotkey; "Recent errors" table; log path + Reveal. Same data as today.

**Settings**
```
┌ Settings ─────────────────────────────────────────────┐
│ Library  │ Clips folder  [C:\…\clips        ] [Browse] │
│ Capture  │ ── Capture ─────────────────────────────── │
│ Auto rec │ FPS (60) Output (1080p) Quality Bitrate     │
│ Games    │ Monitor / window   … [Apply capture]        │
│          │ (sections scroll; sub-nav highlights)       │
└──────────┴─────────────────────────────────────────────┘
```
Section headings keep their exact text (`Library`, `Capture`, `Automatic recording`,
`Game integrations`) and every "Apply …" button keeps its label — shots locates them by role+name.

**Recorder bar** (replaces `RecordingPanel` in the sidebar). Left: state pill — `Idle` (neutral),
`Watching for games` (accent outline), `● REC Buffer` / `● REC Session` (red dot, 1.6s pulse,
static under reduced motion), `Error` (danger, click → Health). Middle: span / fps / dropped as
compact tabular numbers. Right: primary `Save clip` with the hotkey chord as a `<kbd>`, `Start` /
`Stop`, and a `▾` popover holding today's `<details class="engine">` engine stats. The pill is
`role="status"` with the same text the panel announces today.

**Banners → toasts.** Error/notice move to a bottom-right stack (`role="alert"` / `role="status"`,
keep class `banner error|notice` so shots' 8 `.banner` checks keep working), auto-dismiss notices
after 6s, errors stay until dismissed.

## 3. Design system

All tokens on `:root` in `app.css`, named by role (never by hue) so a light theme is a second
block later, not a rewrite.

**Colour** (dark; every pair is re-verified by `auditContrast`, numbers below are targets)

| Token | Value | Use |
|---|---|---|
| `--bg` | `#0c0d12` | window, rail |
| `--surface-1` | `#13151c` | views, top bar |
| `--surface-2` | `#1a1d26` | cards, inputs, panels |
| `--surface-3` | `#232735` | hover, popovers, menus |
| `--border` | `#2a2f3d` | hairlines |
| `--border-strong` | `#3a4052` | inputs, focused cards |
| `--text` | `#eceef4` | body (≈16:1 on surface-1) |
| `--text-muted` | `#a3aabb` | metadata (≈7.5:1 on surface-2) |
| `--text-faint` | `#7d8496` | timestamps, large text only (≈4.6:1 on surface-1) |
| `--accent` | `#8b7bff` | links, focus ring, selection, active rail item (≈5.6:1) |
| `--accent-strong` | `#6a58f2` | primary button fill (white text ≈5.3:1) |
| `--accent-soft` | `rgb(139 123 255 / 0.14)` | selected card / active rail backdrop |
| `--rec` | `#ff4d5e` | live-recording dot and pill only |
| `--danger` | `#f87171` | destructive actions, errors |
| `--warn` | `#fbbf24` | warnings, storage over cap |
| `--ok` | `#4ade80` | healthy rows, cap met |
| `--star` | `#fbbf24` | favourite |

Accent choice: a violet-indigo. Teal reads as "2019 dashboard" and sits close to `--ok` green, so
"selected" and "healthy" were hard to tell apart. Violet is clear of every status hue
(red/amber/green), keeps `--rec` red reserved for "recording now", and stays legible on near-black.
Pick the exact value with `auditContrast`, not by eye.

**Elevation.** Dark UIs elevate with lighter surfaces, not shadows: surface-1 → 2 → 3, plus one
shadow token for floating layers only: `--shadow-pop: 0 8px 24px rgb(0 0 0 / .45), 0 0 0 1px
var(--border)` (menus, popover, toasts).

**Radii.** `--r-sm 6px` (chips, inputs), `--r-md 10px` (buttons, cards), `--r-lg 14px` (panels,
player), `--r-full 999px` (pills).

**Spacing.** 4px base: `--s-1 4, --s-2 8, --s-3 12, --s-4 16, --s-5 24, --s-6 32, --s-7 48`.
View padding `--s-5`; card gap `--s-4`.

**Type.** **Inter** (variable, SIL OFL 1.1) for UI, **JetBrains Mono** (variable, SIL OFL 1.1)
for paths, timecodes, codecs. Both installed as `@fontsource-variable/inter` /
`@fontsource-variable/jetbrains-mono` (devDependencies — vite copies the `.woff2` into `dist/`,
served from `'self'`), latin + latin-ext subsets only (~60 KB total). Licence texts go in
`apps/desktop/THIRD_PARTY_NOTICES.md` (new) and are listed in the README's licence section.
`font-feature-settings: 'tnum', 'cv11'` on numeric UI (counters, durations) so digits don't
jitter while recording. Scale (px / line-height / weight):

| Token | Size | Use |
|---|---|---|
| `--t-display` | 24 / 1.25 / 650 | view title |
| `--t-title` | 18 / 1.3 / 600 | page title (clip name, session) |
| `--t-heading` | 15 / 1.4 / 600 | section heading (sentence case — **no more uppercase h2**) |
| `--t-body` | 14 / 1.5 / 400 | default |
| `--t-small` | 12.5 / 1.45 / 450 | metadata, chips |
| `--t-micro` | 11 / 1.3 / 550 | rail labels, duration badge |

**Icons.** Lucide (ISC licence). No npm runtime package: copy the ~25 needed SVG path sets into
`src/lib/icons.ts` (name → path data) rendered by `src/lib/components/Icon.svelte`
(`<svg aria-hidden="true" stroke="currentColor" stroke-width="1.75">`). Decorative by default;
icon-only buttons keep their existing `aria-label`. Licence in `THIRD_PARTY_NOTICES.md`.

**Motion.** `--ease: cubic-bezier(.2,.8,.2,1)`; `--d-fast 120ms` (hover, press), `--d-base 180ms`
(view cross-fade, menu/popover open, toast in), `--d-slow 260ms` (detail page slide-in, 12px).
REC dot pulse 1.6s. Everything wrapped in `@media (prefers-reduced-motion: reduce)` → durations 0,
no pulse. Svelte `fade`/`fly` only for view switches and toasts; no animation on list data so the
recording poll never makes the grid shimmer. shots sets `reducedMotion: 'reduce'` in the page
context so screenshots never catch mid-transition frames.

**Focus.** One rule for every interactive element: `outline: 2px solid var(--accent);
outline-offset: 2px; border-radius: inherit` on `:focus-visible`; cards also get
`box-shadow: 0 0 0 4px var(--accent-soft)`. The rail is a roving-tabindex list (↑/↓), the grid
supports ←/→/↑/↓ between cards (`nav.ts` computes the target index from column count; tested).

**Light theme: recommend NO for this overhaul.** Spec §9 declares dark-only and the whole
`auditContrast` table, `index.html` first frame and window `theme: "Dark"` assume it; the users are
gamers in dark rooms and video previews look best on dark. Building the tokens semantically keeps
a light theme to a ~25-line override block later. Revisit after the overhaul ships (open question 2).

## 4. Component change list

| File | Change |
|---|---|
| `src/app.css` | Replace tokens with §3 set; keep legacy names as aliases during Phase 1–2 (`--panel: var(--surface-1)` etc.), delete aliases in Phase 3. Primitives: button variants (`primary`, `ghost`, `danger`, `icon`), `.chip`, `.card`, `.badge`, `kbd`, inputs/selects, `.view` / `.view-head` layout, focus rule, reduced-motion block, `@font-face` imports. Remove uppercase `h2`. |
| `index.html`, `src-tauri/tauri.conf.json` | `#0b0c10` → new `--bg` hex (only the colour; nothing else in the config). |
| `src/App.svelte` | Becomes the shell: rail + top bar + view switch + toasts. Data loading, polling and every handler stay as they are; markup moves. Selection state → `View` from `nav.ts`. Target < 450 lines. |
| `src/lib/nav.ts` (+test) **new** | `View` type, transitions, health badge predicate, grid key-nav index math. |
| `src/lib/icons.ts`, `components/Icon.svelte` **new** | Bundled Lucide subset. |
| `components/NavRail.svelte` **new** | Rail, roving focus, badge. |
| `components/Toasts.svelte` **new** | Error/notice stack (keeps `.banner` classes + roles). |
| `RecordingPanel.svelte` → `RecorderBar.svelte` | Same props (`status`, `app`, `busy`, `onStart/onStop/onClip`); horizontal bar + engine popover. Keep root class `recorder`. |
| `ClipFilterBar.svelte` | One toolbar row: search with icon, game/tag selects, favourites toggle; same `filter`/`onChange` contract. |
| `ClipList.svelte` | List → card grid (`<ul>` of cards kept, so it stays a list for AT); star/copy/reveal/delete via icon buttons + `⋯` menu; drag-out untouched. |
| `ClipDetail.svelte` | Page header with back, inline-editable title, action toolbar; player + side details column; trim panel restyled (keep `.panel.trim`, `.selection .mono`, `.lossless`). |
| `Timeline.svelte`, `MatchTimeline.svelte` | Skin only: track/handle/marker colours and sizes from tokens, marker icons per event kind. Geometry, hit areas, keyboard nudging and class names (`.timeline`, `.playhead-readout`, `.hint`, `.legend`) unchanged. |
| `SessionList.svelte` | Master list rows: game icon/initial, duration, size, date, favourite. |
| `SessionDetail.svelte` | Header + actions toolbar; player; matches and markers as two bands; marker list as a table with Extract. |
| `StoragePanel.svelte` | Stat cards + usage meter; drop `<details>`; keep `.storage`, `.verdict.ok/.bad`, `dt/dd`, `.footnote`. |
| `HealthPanel.svelte` | Status rows with icons; drop `<details>`. |
| `SettingsPanel.svelte` | Section sub-nav + scrolling sections; drop `<details>`; keep headings/labels verbatim. |
| `src/a11y.test.ts` | Add a case per view (navigate by clicking rail buttons), keep the current case. |
| `scripts/shots.mjs` | See §5 Phase 1/4. |
| `docs/specs/…-design.md` §9 | One-paragraph amendment pointing at this plan (Phase 4). |

## 5. Phased delivery

Phases 0 and 1 are serial (they touch the shared files). Phase 2 is four parallel chunks on
disjoint files. Phases 3–4 are serial. No phase changes behaviour; each is mergeable alone and
leaves the app working.

### Phase 0 — Foundations (1 agent)
Files: `app.css`, `index.html`, `tauri.conf.json` (bg hex), `package.json` (+2 font devDeps),
`lib/icons.ts`, `components/Icon.svelte`, `THIRD_PARTY_NOTICES.md`, no-remote-URL check.
- New tokens + legacy aliases, fonts, focus rule, reduced-motion block, button/input primitives.
- Accept: check/test/shots green; `auditContrast` passes every row; `dist/` contains the woff2 and
  no `http(s)://` in CSS; old layout intact (only re-skinned).
- Risk: font metrics change widths → layout audits on the sidebar may fail at 940. Mitigation:
  fix spacing in `app.css` only; if a component needs change, defer to its Phase 2 owner.

### Phase 1 — Shell + navigation + test hooks (1 agent)
Files: `App.svelte`, `lib/nav.ts`(+test), `NavRail.svelte`, `Toasts.svelte`,
`RecordingPanel.svelte`→`RecorderBar.svelte`, `a11y.test.ts`, `shots.mjs`.
- Build rail, top bar, view switch; mount existing components unchanged inside their views
  (Storage/Health/Settings temporarily still with their `<details>`).
- **shots decoupling (the key enabler for Phase 2):** add stable `data-region` attributes
  (`rail`, `recorder`, `library`, `clip-grid`, `clip-page`, `sessions`, `session-list`,
  `session-page`, `storage`, `health`, `settings`) and rewrite every `.sidebar …` selector (31
  uses, incl. `layoutAudit`'s `clipList`/`sessionList`/`storage`/`recorder` and
  `CONTRAST_LABELS['.sidebar h2']`) to `[data-region=…]`. Each shots state gains a "navigate to
  view X" step. Split shots' states into one exported function per view block if it can be done
  mechanically; otherwise leave the file whole.
- Short-window state (`15-short-window`): the sidebar's "last-resort scroll" rule goes away; the
  view's own container scrolls instead — rewrite that assertion, keep its intent (nothing
  unreachable at 940×600).
- Owns shots states 00, 09–11, 15 (recorder bar, toasts, hotkey, short window).
- Accept: every capability reachable; a11y per-view cases pass; shots green with 00–16 re-shot;
  `nav.test.ts` covers back-after-delete, health badge, grid key-nav.
- Risk: largest diff in the plan. Do it as several commits (nav.ts → shell → shots).

### Phase 2 — Views (4 parallel agents, disjoint files)
Rules for all four: may not touch `App.svelte`, `app.css` (except appending a clearly fenced
block if a new shared primitive is unavoidable — prefer component `<style>`), `nav.ts`, or other
chunks' files. Must keep every `data-region`, every class listed in `CONTRAST_LABELS`, and every
role+name shots locates. Edits to `shots.mjs` are limited to the states of their own view, in a
separate commit, so rebases are mechanical.

| Chunk | Files | shots states owned | Accept (in addition to the global checks) |
|---|---|---|---|
| 2A Library | `ClipList`, `ClipFilterBar` | 01, 02, 13 | Grid 3 cols @940, 4–5 @1280; card keyboard path open/favourite/menu; drag-out pointer path unchanged; empty + filtered-empty states |
| 2B Clip page | `ClipDetail`, `Timeline` | 03–06 | `05-trim-partial` pointer drag still maps to the same ms; handle keyboard nudge unchanged; side column wraps < 1100px |
| 2C Sessions | `SessionList`, `SessionDetail`, `MatchTimeline` | 12 | master–detail at 940 (list 300px); Extract clip/match unchanged; marker table readable |
| 2D System | `StoragePanel`, `HealthPanel`, `SettingsPanel` | 07, 08, 14, 16 | `<details>` removed → shots `… summary` clicks replaced by "navigate to view"; settings headings/Apply labels verbatim; storage verdicts keep classes |

Order inside Phase 2: any; merge in the order they finish, each rebased on main with shots re-run.

### Phase 3 — Polish (1 agent)
Files: `app.css` (remove legacy aliases), `App.svelte` (view transitions, Esc/Alt+← back),
`Toasts.svelte` (auto-dismiss), empty states across views (small edits after Phase 2 merged).
- Accept: no legacy token names left (`grep -- '--panel\b\|--muted\b' src` empty); reduced-motion
  verified in shots (context option) and by a vitest that the class/attr is applied.

### Phase 4 — Docs (1 agent)
README screenshots refreshed from `target/desktop-shots/`; spec §9 amendment; CHANGELOG entry.

### What stays unverified
As today, shots runs headless Chromium, not WebView2. Inter/variable-font rendering and the
backdrop of the popover in WebView2 on Windows, and real-DPI (150%) sizing, are unverified until
someone opens the real window — say so in the CHANGELOG entry.

### Risks
- **shots.mjs coupling** (2367 lines, selector-heavy) — mitigated by Phase 1 `data-region` hooks.
- **Contrast regressions** from new surfaces — `auditContrast` is the gate; never drop labels.
- **Polling churn**: `recording` status polls; the top bar must not re-mount children on each
  tick (keyed blocks only on view identity) or the popover/focus will reset.
- **Big thumbnails = full-res JPEG decode** in a 48-clip grid: `loading="lazy"` +
  `decoding="async"`; if memory is a problem, a backend thumbnail size is a *separate* follow-up
  (it would touch Rust — out of scope here).
- **Parallel merge conflicts in shots.mjs** — per-view state ownership + separate commits.

## 6. Decisions for the user (defaults chosen; the plan proceeds with them)

1. **Navigation: left icon rail (recommended) vs top tabs.** Rail: keeps vertical space for the
   16:9 player on a 880px window and scales to 5 items with a health badge. Top tabs cost ~44px
   of height the player needs.
2. **Light theme: no for now (recommended).** Tokens make it cheap later; doing it now doubles
   the contrast audit and contradicts spec §9 for little user value in a gaming app.
3. **Accent: violet `#8b7bff` (recommended) vs keep teal.** Teal collides with "healthy" green
   and is half of why it looks dated; violet is clear of all status hues.
4. **Font: bundled Inter + JetBrains Mono, ~60 KB (recommended) vs keep `system-ui`.**
   system-ui renders Segoe UI on Windows, which is fine but is exactly the "default app" look;
   bundled fonts are offline, OFL-licensed, and give tabular digits for live counters.
