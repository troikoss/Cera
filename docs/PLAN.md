# Cera — implementation plan

How we get from an empty repo to a daily-usable app. Architecture and reasoning live in
[DESIGN.md](DESIGN.md); this file is the work list.

**Workflow:** Claude implements one milestone at a time; you review the code and test on the
tablet; the next milestone starts after your feedback. Each milestone ends with something you
can run, plus a short "test on the tablet" list.

**Status:** M0–M2 done and tested on the tablet. M3 implemented; opens on the tablet, waiting
for your scroll/zoom test. Still to record from the tablet: the M1 stylus log and renderer choice.

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
- [ ] Try `GSK_RENDERER=gl`, `vulkan`, `cairo` and note frame times (GTK 4.18 removed the old GL
      renderer and renamed `ngl` to `gl`)

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

- [ ] Input routing: stylus → tool, eraser end → eraser, barrel button → temporary eraser,
      mouse → draw at fixed pressure, touch → pan/zoom
- [ ] Palm rejection: swallow touch while the pen is in proximity and for 500 ms after
- [ ] Wet-ink buffer with incremental cairo drawing and partial texture updates
      (`MemoryTextureBuilder::set_update_region`)
- [ ] Pen outline generation (pressure → radius, round caps) and highlighter polyline
- [ ] Pen-up: RDP simplify, insert into model + per-page `rstar` tree, re-render touched ink tiles,
      clear wet ink once they arrive
- [ ] Stroke eraser (R-tree query + segment distance test)
- [ ] Undo/redo wired to the header buttons and Ctrl+Z / Ctrl+Shift+Z
- [ ] Debounced autosave (~2 s after last edit, on tab close, on quit)
- [ ] Minimal floating tool bar: `AdwToggleGroup` (pen, highlighter, eraser) + 3 colour swatches
- [ ] Benchmarks (criterion): outline generation, tile render with 500 strokes

**Test on the tablet:** fill a few pages of handwriting over a PDF; erase; undo/redo; `kill -9`
mid-writing and reopen.

**Done when:** writing feels immediate and a kill loses at most the last ~2 s.

## M5 — Vault

- [ ] Open folder / create vault; recent vaults in `~/.config/cera/vaults.json`
- [ ] `.cera/` per-vault folder: `vault.toml`, `workspace.json`
- [ ] Sidebar tree: `DirectoryList` → `FilterListModel` → `SortListModel` → `TreeListModel` →
      `ListView` + `TreeExpander`
- [ ] New note (default paper), new folder, inline rename, drag-and-drop move, delete to trash
- [ ] Import PDF dialog (`AdwDialog`: name, folder, page range, keep original) + drag-and-drop
      onto sidebar/window; "Convert to note" when opening a `.pdf`
- [ ] Tabs (`AdwTabView` + autohide `AdwTabBar`); inactive tabs drop tiles
- [ ] Restore open tabs and last page per note
- [ ] Sidebar filename search
- [ ] Toasts for import, delete, errors

**Test on the tablet:** set up one real subject folder and use it for a day.

## M6 — Pages and tools

- [ ] Page sorter (`AdwDialog` / `AdwBottomSheet` + `GtkGridView`): reorder, insert blank
      before/after, delete, change paper
- [ ] Add-page button in the tool bar
- [ ] Lasso: select, move, delete, recolour, copy/paste between notes
- [ ] Tool popovers: width presets, pressure toggle; colour palette (`AdwWrapBox`) + `GtkColorDialog`
- [ ] Two-/three-finger tap for undo/redo; "draw with finger" setting
- [ ] Preferences dialog (default paper and size, tile budget, gestures)
- [ ] Keyboard shortcuts dialog

## M7 — Export and packaging

- [ ] Vector PDF export: cairo `PdfSurface`, Poppler `render_for_printing`, strokes as paths
- [ ] PNG export of the current page
- [ ] Flatpak manifest (GNOME runtime) for x86_64 and aarch64
- [ ] `.desktop` file, AppStream metainfo, MIME type for `.cera`, app icon
- [ ] Licence headers / `COPYING` (GPL-3.0-or-later)

## Later

Partial (area) eraser, shapes and ruler, text boxes, images, PDF text search, handwriting
recognition, dark paper, sync-conflict handling.

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
- Default paper and page size for new notes (A4 ruled?)
