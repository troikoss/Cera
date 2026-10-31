# Cera — design and plan

> App id `io.github.troikoss.Cera`, binary `cera`, file extension `.cera`. Named after *cera*, the wax
> of Roman writing tablets you wrote on with a stylus.

A handwritten-notes app for Linux tablets, written in Rust with GTK4 and libadwaita.
The hard requirement is efficiency: it must run smoothly on a ~4 GB ARM tablet with a
Mali GPU sharing system memory.

## 1. Requirements

| Area | Decision |
| --- | --- |
| Input | 2-in-1 touchscreen + active stylus: pressure, eraser end, finger pan / pinch-zoom, palm rejection |
| Surface | Fixed-size pages (A4/Letter/PDF page size), scrolling vertically |
| PDF | Import converts a PDF into a native note; PDF pages become page backgrounds |
| PDF storage | Original PDF bytes embedded once in the note file, rendered on demand |
| Organisation | Obsidian-style vault: any folder on disk, sidebar file tree, notes are files |
| Look | libadwaita, minimal chrome, floating tool bar |
| Licence | GPL-3.0-or-later (required by Poppler) |
| Target | ARM tablet, ~4 GB RAM, Mali GPU, Wayland (developed on x86 Ubuntu 26.04 / KDE) |

## 2. Prior art and what we do differently

Rnote (Rust, GTK4, libadwaita) is the closest existing app. Its design choices that
cost memory on small devices, and our alternative:

| Rnote | Cera |
| --- | --- |
| Every stroke keeps its own rendered bitmaps and GSK render nodes (`RenderComponent { images, rendernodes }` per stroke), so memory and node count grow with the number of strokes | Per-*tile* caches: memory grows with screen area, not with how much you've written |
| Its deps (`hayro`, `hayro-svg`, `librsvg`, `usvg`) suggest PDF pages go PDF → SVG → rasterise | Poppler rasterises straight into tile buffers from a memory-mapped PDF |
| Infinite canvas, whole document serialised on save | Pages + append-only binary chunks; autosave writes only changed pages (KBs) |
| Undo by storing store snapshots | Undo stores diffs (`Arc`-shared strokes, so removed strokes cost a pointer) |

Xournal++ (C++, GTK3, cairo, Poppler) is the other reference: lighter, page-based, but
GTK3 and a gzip-XML format that is rewritten in full on save.

## 3. Stack

Versions checked 2026-10-31.

| Purpose | Crate / lib | Version | Note |
| --- | --- | --- | --- |
| UI toolkit | `gtk4` | 0.11.5 | feature `v4_18` (system has 4.22.2; 4.18 keeps Debian 13 / Mobian working) |
| GNOME widgets | `libadwaita` (as `adw`) | 0.9.2 | feature `v1_7` for `AdwToggleGroup`, `AdwWrapBox` (system has 1.9.1) |
| PDF | `poppler-rs` | 0.26.0 | binds poppler-glib; uses cairo-rs/glib 0.22, same as gtk4 0.11 |
| 2D raster | `cairo-rs` (via `gtk4::cairo`) | 0.22 | strokes and Poppler share one rasteriser (pixman has NEON paths on ARM) |
| Spatial index | `rstar` | 0.13 | per-page R-tree of stroke bounding boxes (eraser, lasso, tile invalidation) |
| File access | `memmap2`, `crc32fast` | latest | mmap the note file; checksum chunks |
| Manifest encoding | `serde` + `postcard` | latest | compact, versioned |
| Threads | `std::thread` + `async-channel` | latest | results awaited on the GTK main loop with `glib::spawn_future_local` |
| Errors / logs | `thiserror`, `anyhow`, `tracing` | latest | |
| Resources | `glib-build-tools` | 0.22 | compiles `.ui`, CSS and icons into a GResource in `build.rs` |

No async runtime (tokio), no WebKit, no librsvg.

System packages for development (Ubuntu 26.04):

```bash
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev libpoppler-glib-dev
```

## 4. Architecture

```
cera/                   Cargo workspace
├─ crates/core          no GTK. Model, file format, geometry, stroke outlines, undo.
├─ crates/render        cairo + Poppler tile rasteriser and worker thread. No widgets.
├─ crates/app           GTK/libadwaita UI: window, vault sidebar, canvas widget, input.
└─ crates/cli           cera-cli: import / info / compact / render-page (testing aid)
```

`core` is pure Rust so the format, geometry and undo logic are unit-testable and fuzzable
without a display.

### Threads

| Thread | Owns | Does |
| --- | --- | --- |
| Main (GTK) | widgets, document model, tile cache | input, edits, snapshot (only appends cached textures) |
| Render worker (1, maybe 2 on 4+ cores) | its own `poppler::Document`s, cairo surfaces | `TileJob` in → RGBA buffer out |
| Save | — | appends dirty chunks, fsync, occasional compaction |

Poppler objects are not `Send`, so each worker opens its own `poppler::Document` from the
shared `glib::Bytes` (which is thread-safe). Strokes are immutable `Arc<Stroke>`, so a job
carries the strokes it needs without copying point data.

Every job carries a generation number; jobs from an old zoom level or a closed tab are
dropped on arrival.

## 5. Rendering and memory model

The canvas is a custom widget implementing `GtkScrollable` inside a `GtkScrolledWindow`
(free scrollbars and kinetic touch scrolling).

### Layers per page

1. **Paper** — blank / ruled / grid / dots. Drawn with `append_color` rectangles every frame.
   Zero textures, effectively free on the GPU.
2. **PDF background** — tiles rendered by Poppler. Only for PDF pages.
3. **Ink** — tiles rendered by cairo from the page's strokes, transparent background.
   Highlighter strokes are drawn first inside each tile so pen ink always sits on top.
4. **Wet ink** — the stroke being drawn right now (see below).

### Tiles

- 256 × 256 device pixels, RGBA → 256 KiB each.
- Zoom is quantised to steps of 2^(1/4) (≈ 1.19×). While pinching, existing tiles are scaled by
  the GPU; new tiles are requested 150 ms after the gesture settles.
- At low zoom a whole page is one texture instead of many tiles (fewer nodes, and scanned PDFs
  decode their image once instead of once per tile).
- Cache key `(doc, page, layer, zoom step, tx, ty)` → `gdk::Texture`, LRU with a **byte budget**
  (default 48 MiB, configurable). Visible tiles first, then one ring of prefetch.
- Textures are kept as stable `gdk::Texture` objects so GSK uploads each one once, not per frame.
- No retained cairo surfaces: a tile is rendered, turned into a `gdk::MemoryTexture`, and the
  buffer handed over. An ink edit re-renders affected tiles from the R-tree query (a few ms).
- `gio::MemoryMonitor` low-memory warnings drop everything except visible tiles.
- Inactive tabs release all their tiles; their strokes stay (they're small).

Rough numbers for a 1280 × 800 screen: ~20 visible tiles per layer + prefetch ring ≈ 10 MiB
per layer. The 48 MiB budget covers two layers with headroom.

### Wet ink (low-latency drawing)

- One viewport-sized RGBA buffer (≈ 4 MiB at 1280 × 800).
- Each motion event draws only the new segment into it with cairo.
- The texture is rebuilt with `gdk::MemoryTextureBuilder::set_update_texture` +
  `set_update_region` (GTK ≥ 4.16), so the renderer uploads only the dirty rectangle per frame.
- On pen-up: simplify (Ramer–Douglas–Peucker, ε ≈ 0.05 pt, pressure-aware), insert into model
  and R-tree, request re-render of the touched ink tiles. The wet-ink pixels stay visible until
  those tiles arrive, then that region is cleared — no flicker.

### Stroke geometry

- Pen: variable-width outline polygon from per-point pressure radius (perfect-freehand style),
  round caps, filled once → correct edges, no overlapping-alpha artefacts.
- Highlighter: constant-width polyline, round joins, ~35 % alpha.
- Coordinates are page points (1/72 in), so ink lines up with PDF pages exactly.

## 6. Input

| Source | Controller | Behaviour |
| --- | --- | --- |
| Stylus tip | `GestureStylus`, `stylus_only = true` | draw with active tool; points from `backlog()` + current event (GTK coalesces motion events, the backlog has the full-rate samples); pressure from `axis(Pressure)` |
| Stylus eraser end | same, `device_tool().tool_type() == Eraser` | eraser regardless of selected tool |
| Stylus barrel button | same, button mask | hold → temporary eraser (configurable: lasso) |
| One finger | `GtkScrolledWindow`'s own touch pan | scroll with kinetic momentum |
| Two fingers | `GestureZoom` | pinch zoom around the centroid |
| Two-/three-finger tap | `GestureClick` (touch) | undo / redo (setting) |
| Mouse | `GestureDrag` button 1 | draw at fixed pressure; middle-drag pans |
| Wheel | `EventControllerScroll` | scroll; Ctrl + wheel zooms |

**Palm rejection:** while the stylus is in proximity and for 500 ms after it leaves, a
capture-phase touch controller claims and swallows touch sequences so a resting hand neither
scrolls nor draws. Optional "draw with finger" setting turns one-finger touch into drawing.

**Latency targets:** < 1 ms work per motion event, < 8 ms per frame on the target tablet.

## 7. File format (`.cera`)

One self-contained file per note. Append-only log so autosave is cheap and crash-safe.

```
┌───────────────────────────────────────────────┐
│ Header (64 B)  magic "CERANOTE", version, flags │
├───────────────────────────────────────────────┤
│ Asset blobs   original PDF bytes, 4 KiB-aligned │  ← mmap'd and given to Poppler as-is
├───────────────────────────────────────────────┤
│ Chunk log     [kind u8][len u32][crc32][payload]│
│   PageInk     page id + encoded strokes         │
│   Thumbnail   first page, small PNG/WebP        │
│   Manifest    page order, page specs, assets,   │
│               offset of each page's latest ink  │
│   ... later chunks appended on each save ...    │
├───────────────────────────────────────────────┤
│ Footer (32 B)  manifest offset, generation, crc │  ← always at EOF
└───────────────────────────────────────────────┘
```

- **Open:** read footer → manifest → nothing else. Page ink is decoded lazily when a page
  approaches the viewport. If the footer is bad (crash mid-save), scan backwards for the
  last valid footer.
- **Save:** append changed `PageInk` chunks + new `Manifest` + footer, then fsync.
  Debounced autosave ~2 s after the last edit and on tab close.
- **Compact:** when dead bytes exceed 50 % (or on demand), write a fresh file to a temp
  path and atomically rename it. Existing mmaps stay valid because they point at the old inode.
- **Page spec:** size in pt, paper (blank / ruled / grid / dots + spacing, colour), optional
  `Pdf { asset, page_index }` background.
- **Stroke encoding:** tool, RGBA, base width, point count; first point absolute, then
  zig-zag varint deltas in 1/64 pt; pressure as u8. Typically 3–5 bytes per point on disk.
  In memory: `Box<[Point { x: f32, y: f32, p: f32 }]>` for the pages that are loaded.
- **Versioning:** header version + postcard manifest with an explicit version field;
  unknown chunk kinds are skipped, so newer files mostly open in older builds.
- **Known risk:** a sync tool that rewrites the file *in place* (rather than replace-by-rename)
  while it's mmap'd can crash the app (SIGBUS). Most sync tools rename. Revisit if it bites.

### PDF import

1. Read the PDF bytes; get page count and page sizes with Poppler.
2. Write a new `.cera` file: PDF as an asset blob, one page per PDF page with
   `Pdf { asset, page_index }` background, empty ink, a thumbnail.
3. No rasterisation at import, so it's fast and the file is only slightly bigger than the PDF.
4. Options in the import dialog: name, destination folder, page range, keep or trash the original.

Deleting or reordering pages only edits the manifest; unused assets are dropped on compaction.

## 8. Vault

- A vault is any folder. Per-vault state lives in a hidden `.cera/` folder at its root
  (like `.obsidian/`): `vault.toml` (default paper, tool presets), `workspace.json`
  (open tabs, last page per note, sidebar expansion).
- Recent vaults: `~/.config/cera/vaults.json` (XDG config, no GSettings schema to install).
- Sidebar tree, almost no custom code:
  `gtk::DirectoryList` (monitored) → `FilterListModel` (folders, `*.cera`, `*.pdf`, no dotfiles)
  → `SortListModel` (folders first, natural sort) → `TreeListModel` → `ListView` + `TreeExpander`.
- File operations: new note, new folder, rename inline, drag-and-drop move, delete to the
  desktop trash (`gio::File::trash`).
- A `.pdf` in the vault shows with a PDF icon; opening it offers "Convert to note".
- Thumbnails come from the note's own `Thumbnail` chunk, so no separate cache.
- Search v1: filename filter. Later: PDF text (Poppler) indexed at import.

## 9. UI

Widget tree. Main window: vault tree on the left; header bar with title and page counter; pages on a grey canvas; floating pill tool bar at the bottom centre.

```
AdwApplicationWindow
└─ AdwToastOverlay
   └─ AdwOverlaySplitView                  (AdwBreakpoint < 600sp → sidebar overlays)
      ├─ sidebar: AdwToolbarView
      │  ├─ top: AdwHeaderBar [vault menu ▾] [+ menu: Note, Folder, Import PDF…] [search]
      │  │       GtkSearchBar
      │  └─ GtkScrolledWindow > GtkListView (vault tree)
      └─ content: AdwToolbarView
         ├─ top: AdwHeaderBar [sidebar] AdwWindowTitle("Lecture 03", "Page 2 of 18")
         │                    [undo] [redo] [pages] [menu]
         │       AdwTabBar (autohide)
         └─ AdwTabView → per tab: GtkOverlay
            ├─ GtkScrolledWindow > Canvas
            └─ floating pill (bottom centre):
               AdwToggleGroup [pen | highlighter | eraser | lasso] │ 3 colour swatches │ [add page]
```

- **Tools:** tap the active tool again → popover with width presets and pressure toggle.
  Long-press a swatch → palette (`AdwWrapBox`) + `GtkColorDialog`.
- **Pages button:** `AdwDialog` (or `AdwBottomSheet` when narrow) with a `GtkGridView` of page
  thumbnails: drag to reorder, insert blank before/after, delete, change paper.
- **Import:** drop a PDF on the sidebar/window or use + → Import PDF… → `AdwDialog` with
  `AdwEntryRow` (name), `AdwComboRow` (folder), page range, keep original.
- **No vault open:** `AdwStatusPage` "Open a vault" with "Open folder" / "Create vault" and a
  recent-vaults list.
- **Toasts:** "Imported 18 pages", "Page deleted · Undo".
- **Dark mode:** UI follows `AdwStyleManager`; paper stays white by default (dark paper is a
  later option).
- **Keyboard:** Ctrl+Z / Ctrl+Shift+Z, P / H / E / L for tools, Ctrl+± and Ctrl+0 (fit width),
  F9 sidebar.
- Touch targets ≥ 40 px in the tool bar.

## 10. Memory budget and measurement

Set budgets relative to the measured baseline (GTK + libadwaita alone is a large share):

| Scenario | Target |
| --- | --- |
| Vault open, no note | baseline + ≤ 15 MiB |
| 300-page PDF note, scrolling and zooming | baseline + ≤ 80 MiB RSS |
| GPU textures | ≤ tile budget (48 MiB default) + wet-ink buffer |

Measured baseline (release build, window open on the "Open a vault" page, idle):

| Machine | RSS | PSS | Notes |
| --- | --- | --- | --- |
| x86 desktop, KDE Wayland, default GSK renderer | 105 MiB | 46 MiB | M0, 2026-10-31; PSS is the fairer number since GTK's shared libs count in full in RSS |
| Target tablet | — | — | to record in M0 testing |

How we check:

- `CERA_DEBUG=1` overlay: tile count / bytes per layer, worker queue length, frame time.
- `/proc/<pid>/smaps_rollup` and `heaptrack` on the tablet for each milestone.
- Try `GSK_RENDERER=ngl`, `vulkan` and `gl` on the tablet's Mesa driver and keep the best as
  the documented default.

## 11. Plan

Milestones, task lists, risks and open questions are in [PLAN.md](PLAN.md).
