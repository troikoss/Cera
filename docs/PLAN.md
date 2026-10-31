# Cera — implementation plan

How we get from an empty repo to a daily-usable app. Architecture and reasoning live in
[DESIGN.md](DESIGN.md); this file is the work list.

**Workflow:** Claude implements one milestone at a time; you review the code and test on the
tablet; the next milestone starts after your feedback. Each milestone ends with something you
can run, plus a short "test on the tablet" list.

**Status:** planning done, M0 not started.

## M0 — Skeleton

- [ ] Install system packages (`libgtk-4-dev libadwaita-1-dev libpoppler-glib-dev`)
- [ ] Cargo workspace: `crates/core`, `crates/render`, `crates/app`, `crates/cli`
- [ ] Release profile tuned for size/speed (`lto = "thin"`, `codegen-units = 1`, `panic = "abort"`)
- [ ] `build.rs` compiling a GResource (`.ui` templates, `style.css`, icons) with `glib-build-tools`
- [ ] `AdwApplication` + `AdwApplicationWindow` with `AdwOverlaySplitView`, empty sidebar,
      `AdwStatusPage` "Open a vault"
- [ ] `tracing` logging controlled by `RUST_LOG`
- [ ] `cargo fmt`, `cargo clippy -D warnings`, `cargo test` all clean

**Test on the tablet:** builds natively; window opens; sidebar collapses below 600sp.
Record baseline RSS (`grep -E 'Rss|Pss' /proc/$(pidof cera)/smaps_rollup`).

**Done when:** the window runs on desktop and tablet and the baseline is written into DESIGN.md §10.

## M1 — Stylus spike

- [ ] Throwaway `examples/stylus.rs` in `crates/app`: one `GtkDrawingArea`-style widget
- [ ] `GestureStylus` (`stylus_only`): log down/motion/up/proximity, pressure, tilt,
      `DeviceToolType`, button mask, backlog length per motion event, event timestamps
- [ ] Touch: log touch sequences separately; check the stylus never arrives as touch
- [ ] Draw raw polylines (width from pressure) to eyeball latency
- [ ] Try `GSK_RENDERER=ngl`, `vulkan`, `gl` and note frame times

**Test on the tablet:** write a few lines; flip to the eraser end; press the barrel button;
rest your palm while writing; note which renderer feels best.

**Done when:** we know what the hardware reports, and DESIGN.md §6 is corrected if anything differs.

## M2 — Core and file format (`crates/core`, `crates/cli`)

- [ ] Types: `Document`, `PageSpec`, `Paper`, `Background`, `Stroke`, `Point`, `Tool`, `Rgba8`
- [ ] Stroke encoder/decoder (absolute first point, zig-zag varint deltas in 1/64 pt, u8 pressure)
- [ ] Container: header, 4 KiB-aligned asset blobs, chunk log with CRC32, manifest, footer
- [ ] Open: footer → manifest; lazy `PageInk` decode; backward scan on a bad footer
- [ ] Save: append dirty chunks + manifest + footer, fsync; compaction via temp file + rename
- [ ] PDF import (Poppler in `crates/render`, called from the CLI): page count, sizes, embed bytes
- [ ] Undo/redo command log (`AddStrokes`, `RemoveStrokes`, `TransformStrokes`, page ops), capped
- [ ] `cera-cli import <pdf> <out.cera>`, `info <file>`, `compact <file>`
- [ ] Tests: encode/decode round-trip (proptest), save/open round-trip, truncate-at-every-byte
      recovery, compaction keeps content identical

**Done when:** all tests pass and `cera-cli info` on an imported 300-page PDF prints correct
page sizes instantly.

## M3 — Viewer (`crates/render`, canvas in `crates/app`)

- [ ] Canvas widget implementing `GtkScrollable`; vertical page layout with gaps, centred
- [ ] Paper layer drawn with `append_color` (blank, ruled, grid, dots)
- [ ] Render worker thread: `TileJob` / generation counter / cancellation, own Poppler documents
      opened from the mmap'd asset
- [ ] Tile cache: 256 px tiles, quantised zoom steps (2^(1/4)), LRU with byte budget,
      whole-page texture at low zoom, prefetch ring
- [ ] Pinch zoom (`GestureZoom`) and Ctrl+wheel zoom around the pointer; GPU-scaled tiles during
      the gesture, re-render 150 ms after it settles
- [ ] `gio::MemoryMonitor` low-memory handling
- [ ] `CERA_DEBUG=1` overlay: tiles and bytes per layer, queue length, frame time
- [ ] Temporary "open file" action so M3 is testable before the vault exists

**Test on the tablet:** open a 300-page text PDF and a 50-page scanned PDF; fling-scroll top to
bottom; pinch in and out; watch the debug overlay and RSS.

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
2. **GPU driver behaviour** (M1/M3): many small textures on Mesa's Mali driver; renderer choice.
3. **Scanned PDFs** (M3): Poppler decodes a page's full image per render call; whole-page
   textures at low zoom should mitigate it. Measure with a scanned 50-page PDF.
4. **Build times on ARM:** the first gtk-rs build on a Cortex-A55-class CPU is slow. Develop on
   x86, build release on the tablet over SSH (incremental builds are fine), Flatpak for installs.

## Open questions

- Which tablet exactly (affects M1 and the renderer choice)
- Default paper and page size for new notes (A4 ruled?)
