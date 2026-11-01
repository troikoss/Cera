# Cera — implementation plan

How we get from an empty repo to a daily-usable app. Architecture and reasoning live in
[DESIGN.md](DESIGN.md); this file is the work list.

**Workflow:** Claude implements one milestone at a time; you review the code and test on the
tablet; the next milestone starts after your feedback. Each milestone ends with something you
can run, plus a short "test on the tablet" list.

**Status:** M0–M7 implemented. M0–M3 tested by you on the tablet. M4–M7 checked on the tablet
with real pointer, keyboard and touch input injected into a headless compositor
(`tools/headless`): drawing, two-/three-finger undo/redo, pinch, draw with finger, lasso
(select, move, recolour, copy, paste, delete, undo), sidebar rename and Move To, search,
import, tool and palette popovers, page sorter. The Flatpak builds against GNOME 51 and runs.
Still yours to do: the stylus (it can't be injected) and a day of real use; the M1 stylus log.
First release, 2026.11.1 (2026-11-01): Linux tarballs for x86_64, i686 and aarch64 built in
Debian 13 containers (`build-aux/release-linux.sh`); Windows needs a port of `core::file` first.
After the release, the D1 roadmap items (2026-11-01): editable size slots, a colour wheel,
slot counts in preferences, rectangle select, layer order for a selection; then the first
D2 items: pen (stroke-touch) select as a third lasso mode, page bookmarks (manifest
version 2) and add space as a fourth lasso mode. The lasso lost its swatches; a selection is
recoloured from Change style in the context menu. The page sorter moved from its dialog into
the sidebar, as a view next to the files, with a context menu and a bookmark toggle on each
thumbnail and a filter for bookmarked pages; the header's bookmarks menu is gone. Add space
now spills ink pushed off a page onto the next one, which makes room for it, cascading to
a new page at the end. Checked with injected pointer and key input in the headless
compositor; yours to test on the tablet.

## M0 — Skeleton

- [x] Install system packages (`libgtk-4-dev libadwaita-1-dev libpoppler-glib-dev`)
- [x] Cargo workspace: `crates/core`, `crates/render`, `crates/app`, `crates/cli`
- [x] Release profile tuned for size/speed (`lto = "thin"`, `codegen-units = 1`, `panic = "abort"`)
- [x] `build.rs` compiling a GResource (`.ui` templates, `style.css`, icons) with `glib-build-tools`
      (no custom icons yet; GtkApplication picks up `icons/` under the resource base path)
- [x] `AdwApplication` + `AdwApplicationWindow` with `AdwOverlaySplitView`, empty sidebar,
      `AdwStatusPage` "Open a vault"
- [x] `tracing` logging controlled by `RUST_LOG`
- [x] `cargo fmt`, `cargo clippy -D warnings`, `cargo test` all clean

**Test on the tablet:** builds natively; window opens; sidebar collapses below 600sp.
Record baseline RSS (`grep -E 'Rss|Pss' /proc/$(pidof cera)/smaps_rollup`).

**Done when:** the window runs on desktop and tablet and the baseline is written into DESIGN.md §10.

## M1 — Stylus spike

- [x] Throwaway `examples/stylus.rs` in `crates/app`: one `GtkDrawingArea`-style widget
- [x] `GestureStylus` (`stylus_only`): log down/motion/up/proximity, pressure, tilt,
      `DeviceToolType`, button mask, backlog length per motion event, event timestamps
- [x] Touch: log touch sequences separately; check the stylus never arrives as touch
- [x] Draw raw polylines (width from pressure) to eyeball latency
- [x] Try `GSK_RENDERER=gl`, `vulkan`, `cairo` and note frame times (GTK 4.18 removed the old GL
      renderer and renamed `ngl` to `gl`). Tablet, 2026-11-01, `tools/headless/bench.sh` (headless
      mutter at 2560 × 1600 on the real GPU, 5 s scroll at 4000 px/s through 8 pages of the
      324-page note; CPU time per frame from the debug overlay): **Vulkan** 2.2–3.0 ms avg,
      ≤ 19 ms max, 298 MiB RSS; **GL** ~2.5 ms but 349 MiB and a 165 ms stall on the first frame;
      **cairo** ~11 ms, over the 8 ms budget. Keep Vulkan, GTK's default.

Run: `cargo run -p cera --release --example stylus` (flags in the file header). Each pen-up
logs and shows a stroke summary: rates, backlog, pressure/tilt, handler and paint time,
input→presentation latency.

**Test on the tablet:** write a few lines; flip to the eraser end; press the barrel button;
rest your palm while writing; note which renderer feels best.

**Done when:** we know what the hardware reports, and DESIGN.md §6 is corrected if anything differs.

## M2 — Core and file format (`crates/core`, `crates/cli`)

- [x] Types: `Document`, `PageSpec`, `Paper`, `Background`, `Stroke`, `Point`, `Tool`, `Rgba8`
- [x] Stroke encoder/decoder (absolute first point, zig-zag varint deltas in 1/64 pt, u8 pressure)
- [x] Container: header, 4 KiB-aligned asset blobs, chunk log with CRC32, manifest, footer
- [x] Open: footer → manifest; lazy `PageInk` decode; backward scan on a bad footer
- [x] Save: append dirty chunks + manifest + footer, fsync; compaction via temp file + rename
- [x] PDF import (Poppler in `crates/render`, called from the CLI): page count, sizes, embed bytes
- [x] Undo/redo command log (`AddStrokes`, `RemoveStrokes`, `ReplaceStrokes` for transforms and
      recolouring, page ops), capped
- [x] `cera-cli import <pdf> <out.cera>`, `info <file>`, `compact <file>`, plus `scribble <file>`
      to exercise saves
- [x] Tests: encode/decode round-trip (proptest), save/open round-trip, truncate-at-every-byte
      recovery, compaction keeps content identical

**Test on the tablet:** `cargo test --workspace`; import a long PDF and a scanned one with
`cera-cli import`; `info` (page sizes, open time); `scribble` a few times, then `compact`.

**Done when:** all tests pass and `cera-cli info` on an imported 300-page PDF prints correct
page sizes instantly.

On the tablet (2026-11-01): all 27 tests pass; a 324-page PDF imports in 94 ms and opens in
0.09 ms with correct sizes; `scribble` saves of 2–6 MB take 23–190 ms with both fsyncs (ext4
on UFS); compaction 13.9 → 8.2 MiB in 39 ms. Release build of `cera-cli`: 31 s.

## M3 — Note canvas (`crates/render`, canvas in `crates/app`)

Shows a converted note: paper, PDF backgrounds (ink arrives in M4). There is no separate PDF
viewer: opening a PDF converts it to a note first.

- [x] Canvas widget implementing `GtkScrollable`; vertical page layout with gaps, centred
- [x] Paper layer drawn with `append_color` (blank, ruled, grid, dots; patterns as repeat nodes)
- [x] Render worker threads: `TileJob`s, the queue replaced on every scroll (no stale work),
      per-page batching, own Poppler documents opened from the mmap'd asset
- [x] Tile cache: 256 px tiles at the settled zoom, LRU with byte budget, whole-page texture at
      low zoom, prefetch margin, older zoom levels dropped once covered
- [x] Pinch zoom (`GestureZoom`) and Ctrl+wheel zoom around the pointer, Ctrl+± / Ctrl+0; GPU-scaled
      tiles while zooming, re-render 150 ms after it settles
- [x] `gio::MemoryMonitor` low-memory handling
- [x] `CERA_DEBUG=1` overlay: tiles and bytes, queue length, frame time, zoom, RSS
- [x] Temporary Open button (Ctrl+O) and `cera file.pdf`: a PDF is converted to `file.cera`
      next to it (or that note is opened if it exists), then shown

**Test on the tablet:** `CERA_DEBUG=1 cargo run --release -p cera -- some.pdf`; open a 300-page
text PDF and a 50-page scanned PDF; fling-scroll top to bottom; pinch in and out; watch the
debug overlay and RSS.

**Done when:** scrolling stays smooth and memory stays within DESIGN.md §10.

## M4 — Inking

- [x] Input routing: stylus → tool, eraser end → eraser, barrel button → temporary eraser,
      mouse → draw at fixed pressure (middle-drag pans), touch → pan/zoom
- [x] Palm rejection: swallow touch while the pen is in proximity and for 500 ms after
- [x] Wet ink with incremental cairo drawing: tiled on the ink grid, redrawn per event in one
      fill so it matches the finished tiles exactly
- [x] Pen outline generation (pressure → radius, round caps) and highlighter polyline
- [x] Pen-up: RDP simplify, add through `History`, re-render touched ink tiles, clear wet ink
      tile by tile once they arrive (no `rstar`: a linear scan is 4.7 µs for 3000 strokes on the tablet)
- [x] Stroke eraser (bounds check + segment distance), one undo step per drag
- [x] Undo/redo wired to the header buttons and Ctrl+Z / Ctrl+Shift+Z (and Ctrl+Y)
- [x] Debounced autosave (2 s after the last edit, on close, on quit; tabs come in M5)
- [x] Minimal floating tool bar: `AdwToggleGroup` (pen, highlighter, eraser) + 3 colour swatches;
      P / H / E switch tools
- [x] Benchmarks (criterion): outline generation, tile render with 500 strokes, stroke scan

`CERA_DEBUG_DEMO=1` draws, erases and undoes through the real input path with synthetic samples
(for checking without a stylus, e.g. over SSH).

On the tablet (2026-11-01): the demo draws, erases, undoes and autosaves (13 ms with both fsyncs)
at 164 MiB RSS; the ink survives `kill -9`.

**Test on the tablet:** fill a few pages of handwriting over a PDF; erase; undo/redo; `kill -9`
mid-writing and reopen.

**Done when:** writing feels immediate and a kill loses at most the last ~2 s.

## M5 — Vault

- [x] Open folder / create vault; recent vaults in `~/.config/cera/vaults.json`
- [x] `.cera/` per-vault folder: `vault.json`, `workspace.json` (JSON rather than TOML: one
      parser for everything)
- [x] Sidebar tree: monitored folder list (`folder.rs`, not `DirectoryList`, which stops
      updating after a temp-file save) → `FilterListModel` → `SortListModel` → `TreeListModel`
      → `ListView` + `TreeExpander`
- [x] New note (default paper), new folder, inline rename, drag-and-drop move (mouse and pen;
      touch scrolls, so long-press → Move To… instead), delete to trash
- [x] Import PDF dialog (`AdwDialog`: name, folder, page range, keep original) + drag-and-drop
      onto sidebar/window; "Convert to note" when opening a `.pdf`
- [x] Tabs (`AdwTabView` + autohide `AdwTabBar`); inactive tabs drop tiles
- [x] Restore open tabs and last page per note
- [x] Sidebar filename search (Ctrl+F)
- [x] Toasts for import, delete, errors

Notes are compacted when their tab closes if dead bytes outweigh live ink. `CERA_DEBUG=1` adds
an `app.win` action so scripts can run window actions over D-Bus (see `application.rs`).

**Test on the tablet:** set up one real subject folder and use it for a day.

## M6 — Pages and tools

- [x] Page sorter (`AdwDialog`, a bottom sheet when narrow, + `GtkGridView`): reorder, insert
      blank before/after, delete, change paper; thumbnails are whole-page renders from the
      render threads, only for cells on screen, 16 MiB LRU
- [x] Add-page button in the tool bar (Ctrl+Enter)
- [x] Lasso: select, move, delete, recolour, cut/copy/paste between notes
- [x] Tool popovers: tap the active pen or highlighter for width presets and pressure; hold a
      swatch for the palette (`AdwWrapBox`) + `GtkColorDialog`
- [x] Two-/three-finger tap for undo/redo; "draw with finger" setting
- [x] Preferences dialog (paper and size for new notes in the vault, tile budget, gestures)
- [x] Keyboard shortcuts dialog

Single-key shortcuts (tools), Delete, Escape, undo and the clipboard live in a bubble-phase
shortcut controller on the window, so text fields keep those keys.

**Test on the tablet:** lasso a few words and drag them; recolour; copy into another note;
reorder and delete pages in the sorter and undo; two-finger tap to undo; turn on "draw with
finger" and write, then pinch.

## M7 — Export and packaging

- [x] Vector PDF export: cairo `PdfSurface`, Poppler `render_for_printing`, strokes as paths
      (main menu or Ctrl+E; `cera-cli export`)
- [x] PNG export of the current page at 216 dpi (Ctrl+Shift+E; `cera-cli png`)
- [x] Flatpak manifest (GNOME 51 runtime, Poppler 26.10 built in) for x86_64 and aarch64:
      `build-aux/io.github.troikoss.Cera.yml`; built, installed and run on x86_64 (35 MB export,
      7.5 MB installed, 136 MiB RSS with a note open)
- [x] `.desktop` file, AppStream metainfo, MIME type for `.cera`, app icon; `build-aux/install.sh`
      installs them with the binary into `~/.local` (or `PREFIX`)
- [x] Licence headers (SPDX) / `COPYING` (GPL-3.0-or-later), README

On the tablet (2026-11-01): exporting the 324-page note with the M2 scribble ink (6 MiB of
strokes) takes 11 s at 74 MiB peak RSS and writes 50 MiB; the same PDF without ink exports in
about 1 s at its original size. Ink costs ~35 bytes per sample in the PDF (exact pen outlines
as discs and hulls).

**Test on the tablet:** export a written-on PDF note and open it in Papers; export a page as
PNG; `build-aux/install.sh` and start Cera from the app grid; open a `.cera` from Files.

## Later

Partial (area) eraser, shapes and ruler, text boxes, images, PDF text search, handwriting
recognition, dark paper, sync-conflict handling. Feature ideas and add-ons are collected in
[ROADMAP.md](ROADMAP.md).

## Risks to retire early

1. **Stylus support on the actual tablet** (M1): pressure, tool type and proximity through
   Wayland tablet-v2. Everything else depends on it.
2. **GPU driver behaviour** (M1/M3): many small textures on the tablet's Mesa driver (Turnip on
   Adreno 740; GTK picks Vulkan by default); renderer choice.
3. **Scanned PDFs** (M3): Poppler decodes a page's full image per render call; whole-page
   textures at low zoom should mitigate it. Measure with a scanned 50-page PDF.
4. ~~**Build times on ARM**~~ Retired: the Tab S9 builds the workspace tests in ~75 s and an
   incremental release `cera-cli` in ~30 s. Develop on x86, build and test on the tablet over
   SSH (`fedora@<tablet>`, key auth), Flatpak for installs.

## Open questions

- ~~Which tablet exactly~~ Galaxy Tab S9 Wi-Fi on Fedora 44 (DESIGN.md §1). Budgets in §10 were
  relaxed to fit it (2026-11-01): 128 MiB tile budget, 300-page note ≤ baseline + 200 MiB.
- ~~Default paper and page size for new notes~~ A4 ruled, 8 mm; per vault in `.cera/vault.json`.
