# Cera — design and plan

> App id `io.github.troikoss.Cera`, binary `cera`, file extension `.cera`. Named after *cera*, the wax
> of Roman writing tablets you wrote on with a stylus.

A handwritten-notes app for Linux tablets, written in Rust with GTK4 and libadwaita.
The hard requirement is efficiency: it must stay smooth and light on an ARM tablet whose GPU
shares system memory, using far less than Rnote does.

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
| Target | Samsung Galaxy Tab S9 Wi-Fi: Snapdragon 8 Gen 2 (1×X3, 2×A715, 2×A710, 3×A510), Adreno 740 via Mesa Turnip (GTK picks Vulkan), 8 GB (7.6 GB visible), 2560 × 1600 screen at 2× scale, Fedora 44, GNOME on Wayland. Developed on x86 Ubuntu 26.04 / KDE |

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
| Spatial index | — | — | none yet: a linear scan of stroke bounds takes 4.7 µs for a 3000-stroke page on the tablet, so `rstar` waits until lasso or huge pages need it |
| File access | `memmap2`, `crc32fast` | latest | mmap the note file; checksum chunks |
| Manifest encoding | `serde` + `postcard` | latest | compact, versioned |
| Threads | `std::thread` + `async-channel` | latest | results awaited on the GTK main loop with `glib::spawn_future_local` |
| Errors / logs | `thiserror`, `anyhow`, `tracing` | latest | |
| Resources | `glib-build-tools` | 0.22 | compiles `.ui`, CSS and icons into a GResource in `build.rs` |

No async runtime (tokio), no WebKit, no librsvg.

System packages for development (Ubuntu 26.04, then Fedora 44 on the tablet):

```bash
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev libpoppler-glib-dev
```

```bash
sudo dnf install gcc pkgconf-pkg-config gtk4-devel libadwaita-devel poppler-glib-devel
```

## 4. Architecture

```
cera/                   Cargo workspace
├─ crates/core          no GTK. Model, file format, geometry, stroke outlines, undo.
├─ crates/render        cairo + Poppler tile rasteriser and worker thread. No widgets.
├─ crates/app           GTK/libadwaita UI: window, vault sidebar, canvas widget, input.
└─ crates/cli           cera-cli: import / info / compact / export / png / scribble (testing aid)
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
   Each tile draws three passes: highlights under the ink (highlighters from before presets),
   the ink, highlights over it (every highlighter preset). The pass is the stroke style's
   layer; inside a pass, strokes keep page order.
4. **Wet ink** — the stroke being drawn right now (see below).

### Tiles

- 256 × 256 device pixels, RGBA → 256 KiB each (edge tiles are smaller).
- Tiles are rendered at exactly the *settled* zoom, so text is sharp at any zoom. Ctrl+± and the
  zoom buttons move in steps of 2^(1/4) (≈ 1.19×); pinch and Ctrl+wheel are continuous. While
  the zoom changes, the tiles on hand are scaled by the GPU; the visible tiles are requested
  150 ms after it stops. Once a page is covered at the new zoom, its older zoom levels are
  dropped instead of waiting for the LRU.
- A page whose longer side is ≤ 1024 device px is one texture instead of tiles (fewer nodes,
  and scanned PDFs decode their image once instead of once per tile).
- Cache key `(doc, page, layer, scale, tx, ty)` → `gdk::Texture`, LRU with a **byte budget**
  (default 128 MiB). Visible tiles first, then a prefetch margin of 256 device px sideways and
  512 above and below.
- The canvas replaces the worker queue on every scroll, so tiles that scrolled away are never
  rendered. A worker renders a page's queued tiles (up to 16) with one Poppler call and slices
  the result, so a page is parsed (and a scanned image decoded) once per batch, not per tile.
- Textures are kept as stable `gdk::Texture` objects so GSK uploads each one once, not per frame.
  A `gdk::MemoryTexture` keeps its CPU buffer, which is what the budget counts; GSK's GPU copies
  of recently drawn tiles come on top (the same memory on the tablet).
- No retained cairo surfaces: a tile is rendered, turned into a `gdk::MemoryTexture`, and the
  buffer handed over. An ink edit marks the tiles it touches stale (they stay on screen) and
  asks for them again; tiles carry the page's ink generation, so one drawn before a later edit
  is never mistaken for current.
- `gio::MemoryMonitor` low-memory warnings drop everything except visible tiles.
- Inactive tabs release all their tiles; their strokes stay (they're small).

Rough numbers for the 2560 × 1600 screen: 88 visible tiles per layer (22 MiB), 130 with the
prefetch ring (33 MiB). Two layers need ~65 MiB; the 128 MiB budget leaves room to keep the
previous zoom level's tiles while the new ones render.

### Wet ink (low-latency drawing)

Code: `crates/app/src/canvas/wet.rs`.

- Tiled, on the same 256 px grid as the ink tiles, so only tiles the stroke touches exist
  (a full-screen buffer would be 16 MiB, and copying it for every frame's texture too slow).
- Each motion event clears the small pixel-aligned box its new samples touch and redraws the
  stroke's shapes there in one fill, using the render workers' geometry. Wet and finished
  pixels are therefore identical (test: alpha within 2/255); drawing every segment separately
  would add antialiasing coverage at the edges and make wet ink look bolder. One event costs
  ~26 µs on the tablet.
- Dirty wet tiles become new 256 KiB textures once per event; GTK already limits motion events
  to about one per frame, so `snapshot` only appends textures. (No `set_update_region`: a
  tile is small enough to upload whole.)
- Translucent strokes are drawn opaque into the wet tiles and shown with a group opacity, which
  matches one translucent fill.
- On pen-up: simplify (Ramer–Douglas–Peucker, 0.05 pt, pressure 0.03), add through `History`,
  mark the touched ink tiles stale. The wet stroke stays, and each wet tile is hidden as soon as
  the ink tile under it is fresh for the stroke's generation — no flicker, no double edges.

### Stroke geometry

Code: `crates/render/src/ink.rs`.

- Pen: the union of a disc per point (radius from pressure: 25–100 % of half the width) and the
  tangent hull between neighbouring discs, filled once with the winding rule → round caps,
  correct joins, no overlapping-alpha artefacts. Finished strokes skip interior discs where the
  outline turns by less than ~4° (the hulls meet there to within a thousandth of a pixel).
- Square and rounded-square nibs (a style's roundness below 1): the nib is aligned to the page
  axes and sized by pressure like the pen; each segment is the convex hull of its two end nibs
  (corners as 4-segment arcs), all filled once with the winding rule, so opacity stays flat.
- A round highlight at full pressure throughout (a highlighter with pressure off): constant-width
  polyline, round joins and caps. Ink keeps the pen outline at full pressure, which keeps notes
  from before stroke styles pixel-identical.
- Tiles draw only the shapes that reach into them.
- Coordinates are page points (1/72 in), so ink lines up with PDF pages exactly.
- Benchmarks (`cargo bench -p cera-render`), tablet (desktop): 200-sample stroke 348 µs
  (310), a tile with 500 strokes 52 ms (45), one wet-ink event 26 µs (24), finding a tile's
  strokes on a 3000-stroke page 4.7 µs (3). Tablet: a square-nib stroke 122 µs, rounded
  (roundness 0.5) 467 µs; a tile with 500 square-nib strokes 23 ms.

## 6. Input

| Source | Controller | Behaviour |
| --- | --- | --- |
| Stylus tip | `GestureStylus`, `stylus_only = true` | draw with active tool; points from `backlog()` + current event (GTK coalesces motion events, the backlog has the full-rate samples); pressure from `axis(Pressure)` |
| Stylus eraser end | same, `device_tool().tool_type() == Eraser` | the secondary action (a preset, the eraser by default, or the lasso) regardless of selected tool |
| Stylus barrel button | same, button mask (`BUTTON2`/`BUTTON3`) at pen-down | hold → the secondary action; pressed with the tip down, the ink so far stays and the rest of the stroke uses the secondary action (an eraser leaves the stroke so far alone). `GestureStylus` emits *down*/*up* for every button, so a barrel press also arrives as *down* (GDK maps `BTN_STYLUS` to button 2 and `BTN_STYLUS2` to 3, but under GNOME 50 the S Pen's arrives as 3; under X11 the barrel doesn't work): only button 1 starts or ends a stroke. A press mid-stroke resets the gesture (*cancel*), but *motion* and *up* keep coming |
| One finger | `GtkScrolledWindow`'s own touch pan | scroll with kinetic momentum |
| Two fingers | `GestureZoom` | pinch zoom around the centroid |
| Two-/three-finger tap | legacy controller on the tab view (`touch.rs`) | undo / redo (setting); taps are watched, not claimed |
| Mouse | `GestureDrag` button 1 | draw at fixed pressure; middle-drag pans |
| Wheel | `EventControllerScroll` | scroll; Ctrl + wheel zooms |

**Tablet pen** (Galaxy Tab S9): "Wacom WEZ01 S Pen" (`/dev/input/event2`, libinput capability
`tablet`) with pen and eraser tools, one barrel button (`BTN_STYLUS`), pressure 0–4095,
hover distance 0–255, tilt ±63° on both axes, and positions in 1/100 mm over a 236 × 148 mm
area (read with `EVIOCGABS`, 2026-11-01).

**Palm rejection** (`crates/app/src/palm.rs`): a capture-phase controller on the canvas's
overlay swallows touch sequences that begin within 500 ms of the last pen event, or while the
pen is in proximity (capped at 5 s without pen events, in case a proximity-out is lost). Whole
sequences are kept or swallowed, never split. Optional "draw with finger" setting turns
one-finger touch into drawing: `touch.rs` (on the tab view, so palm rejection on its parent
runs first) then takes every touch, draws with one finger, and pans and zooms with two; a
second finger within 250 ms turns a just-started stroke into a pinch.

**Lasso** (`crates/app/src/canvas/lasso.rs`): strokes with more than half of their points
inside the loop (or, in rectangle mode, the dragged rectangle) are selected. In stroke mode
the drag selects every stroke whose ink it passes within 4 px of (segment against segment,
`ink::touches_path`, so a fast swipe can't skip a thin line), and a tap selects the topmost
stroke under it. In add-space mode the press takes nothing until the drag's first move (past
8 px, so a settling pen tip can't pick the wrong side) picks a side: down takes every stroke whose middle is below the press, up every one
whose middle is above it (a press in the gap between pages counts as the edge of the page on
that side, so ink right at an edge can still be moved). The drag moves them only
vertically, away from the line as far as it goes (adding space); dragged back past the press
they rest where they were, so add space never removes space. The dashed line stays where
the press was, with the space being added shown as a band from it. They are deselected on
release. The pages count as one strip
(`History::move_space` in `core/src/space.rs`, one undo step): a stroke mostly past the page's
edge lands on the neighbouring page exactly where it was dropped (pages sit 12 pt apart), or
further on after a long drag; a stroke straddling an edge is nudged just enough to fit the
page holding most of it. A page that receives ink moves its own ink away only as far as
needed to stay 18 pt clear of the arrivals, so it may spill in turn, and blank pages (the
vault's new-page paper) are added at the end, or the start, if needed. An arrival moves on
only if it lies wholly past the edge, so a stroke taller than a page can't loop. Arrivals are
appended, so they draw on top. The selection and lasso layers are drawn after every page, so
a selection dragged past its page shows over the neighbouring one. When an edit or an undo
adds or removes pages, the view scrolls with the page in its middle, so a page added at the
start doesn't move the view off the ink. Selected strokes leave the page's ink tiles and are drawn from
one texture (at most 2048 px, 16 MiB) made when the selection is, so dragging redraws no tile.
A move, recolour or reorder (Arrange: forward, backward, front, back; a step passes the
nearest overlapping stroke in the same layer, since each layer is drawn as a whole;
`core::arrange`) is one `ReplaceStrokes` step; afterwards the strokes rejoin the tiles and
the texture stays as a "ghost" until the tiles have caught up, as wet ink does. The clipboard
is process-wide, so strokes paste into any open note.

**Eraser:** removes whole strokes whose ink comes within the eraser's radius (a size slot,
10 logical px by default) of the eraser (bounds
check, then distance to each segment). One drag is one undo step (`History` groups).

**Latency targets:** < 1 ms work per motion event, < 8 ms per frame on the target tablet.

## 7. File format (`.cera`)

One self-contained file per note. Append-only log so autosave is cheap and crash-safe.

```
┌─────────────────────────────────────────────────────┐
│ Header (64 B)  "CERANOTE", version u32, flags u32, crc │
├─────────────────────────────────────────────────────┤
│ Asset blobs   original PDF bytes, 4 KiB-aligned       │  ← mmap'd and given to Poppler as-is
├─────────────────────────────────────────────────────┤
│ Chunk log     [kind u8][len u32][crc32][payload]      │  crc covers kind, len and payload
│   PageInk     encoding, page id, encoded strokes      │
│   Thumbnail   (kind 2, reserved for M5)               │
│   Manifest    page order, page specs, assets,         │
│               location of each page's latest ink,     │
│               bookmarks                               │
│   ... later chunks appended on each save ...          │
├─────────────────────────────────────────────────────┤
│ Footer (32 B)  "CERAFOOT", generation, manifest       │  ← always at EOF
│                offset + length, crc                   │
└─────────────────────────────────────────────────────┘
```

Code: `crates/core/src/format.rs` (layout), `codec.rs` (strokes), `file.rs` (open/save/compact).

- **Open:** read footer → manifest → nothing else (~0.1 ms for a 324-page note on the tablet). Page ink is
  decoded lazily (`Document::load_ink`) from the mmap. If the last footer is bad (crash
  mid-save), scan backwards for the newest valid one. The bytes after it are left in place and
  counted as dead, never truncated, so a bug can't destroy data on open. A footer from a newer
  format version stops the scan with an error instead of silently falling back.
- **Save:** append changed `PageInk` chunks + new `Manifest`, fsync, then write the footer and
  fsync again, so a footer never points at chunks that didn't reach the disk.
  Debounced autosave ~2 s after the last edit and on tab close (M4).
- **Compact:** due when dead bytes exceed both 1 MiB and the live ink + manifest size (or on
  demand). Writes a fresh file to `.<name>.tmp` and renames it over. Existing mmaps stay valid
  because they point at the old inode. Assets no page uses are dropped, except those the undo
  history could bring back.
- **Page spec:** size in pt, paper (blank / ruled / grid / dots + spacing, line and fill
  colour), optional `Pdf { asset, page }` background.
- **Bookmarks** (manifest version 2; version 1 manifests still open, with none): a list of
  `{ page id, name }` in page order, the name empty unless given. They belong to the page, so
  they follow it when it moves and come back with it on undo, but toggling one is not itself
  an undo step (like a bookmark in a book); it marks the layout dirty and saves like an edit.
- **Stroke encoding** (ink encoding 2): RGBA, style (layer, roundness, pattern, spacing: one
  byte each), base width, point count; points as zig-zag varint deltas in 1/64 pt (the first
  from (0, 0), so absolute); pressure as u8. 3–5 bytes per point on disk. Encoding 1 had a tool
  byte (pen, highlighter) before the colour instead of the style; it is still read, a pen as
  round ink, a highlighter as a round stroke under the ink at full pressure.
  `Stroke::new` rounds to that precision, so a stroke is identical after save and reopen.
  In memory: `Box<[Point { x: f32, y: f32, pressure: f32 }]>` for the pages that are loaded.
- **Versioning:** header version, a version byte in each `PageInk` payload, and a postcard
  manifest whose first field is its version. A golden-bytes test pins the manifest encoding.
  Chunks are only reached through the manifest, so unknown kinds are skipped.
- **Undo** (`crates/core/src/undo.rs`): `History` is the only way to edit ink and pages.
  Commands: `AddStrokes`, `RemoveStrokes`, `ReplaceStrokes` (lasso move/scale, recolour, reorder),
  `InsertPage`, `RemovePage`, `MovePage`, `SetPageSpec`; capped at 500 steps.
- **Known risk:** a sync tool that rewrites the file *in place* (rather than replace-by-rename)
  while it's mmap'd can crash the app (SIGBUS). Most sync tools rename. Revisit if it bites.

### PDF import

1. Read the PDF bytes; get page count and page sizes with Poppler.
2. Write a new `.cera` file: PDF as an asset blob, one page per PDF page with
   `Pdf { asset, page_index }` background, empty ink, a thumbnail.
3. No rasterisation at import, so it's fast and the file is only slightly bigger than the PDF.
4. Options in the import dialog: name, destination folder, page range, keep or trash the original.

Deleting or reordering pages only edits the manifest; unused assets are dropped on compaction.

### Export

Code: `crates/render/src/export.rs`, `page.rs`. The main thread collects every page's spec and
`Arc`-shared strokes (decoding ink not loaded yet) and a worker thread draws them:

- **PDF:** one cairo `PdfSurface` page per note page at its own size: paper as rectangles,
  the background through Poppler's `render_for_printing`, ink as the same filled paths the
  tiles use. Everything stays vector. Ink costs ~35 bytes per sample after compression.
  Bookmarked pages become the PDF's outline ("Page N", or the bookmark's name).
- **PNG:** the current page at 3 px/pt (216 dpi).
- Both write a hidden temp file next to the target and rename it over, so a failed export
  leaves nothing half-written.

## 8. Vault

- A vault is any folder. Per-vault state lives in a hidden `.cera/` folder at its root
  (like `.obsidian/`): `vault.json` (default paper for new notes), `workspace.json`
  (open tabs, the selected one, last page per note). Paths in them are relative to the root,
  so a vault can move. Files are replaced atomically (write a temp file, rename).
- Recent vaults: `~/.config/cera/vaults.json` (XDG config, no GSettings schema to install).
- Sidebar tree: a monitored list per folder (`folder.rs`: `gio::ListStore` + `GFileMonitor`)
  → `FilterListModel` (folders, `*.cera`, `*.pdf`, no dotfiles) → `SortListModel` (folders
  first, natural sort by the shown name) → `TreeListModel` → `ListView` + `TreeExpander`.
  Not `GtkDirectoryList`: in GTK 4.22 its event queue stalls for good when a file is gone
  before its info is read, and every note save that writes a temp file and renames it (a new
  note, a compaction) does that.
- File operations: new note, new folder, rename inline, drag-and-drop move (mouse and pen;
  a touch drag scrolls, so long-press → Move To…), delete to the desktop trash
  (`gio::File::trash`). Rows ask the window to act through `win.*` actions with the path as
  target. Open notes follow renames and moves (`NoteFile::set_path`).
- Tabs: one canvas per tab sharing the window's render threads and tool settings; results
  are routed by note id. Hidden tabs drop their tiles; each note autosaves on its own timer
  and is compacted when its tab closes if due.
- A `.pdf` in the vault shows with a PDF icon; opening it offers "Convert to note".
- Thumbnails come from the note's own `Thumbnail` chunk, so no separate cache.
- Search v1: filename filter. Later: PDF text (Poppler) indexed at import.

## 9. UI

Widget tree. Main window: vault tree on the left; header bar with title and page counter; pages on a grey canvas; floating pill tool bar at the bottom centre (or along the top, left or right edge, a preference); it scrolls when the window is too small for it.

```
AdwApplicationWindow
└─ AdwToastOverlay
   └─ AdwOverlaySplitView                  (AdwBreakpoint < 600sp → sidebar overlays)
      ├─ sidebar: GtkStack
      │  ├─ files: AdwToolbarView
      │  │  ├─ top: AdwHeaderBar [+ menu: Note, Folder, Import PDF…] [vault menu ▾] [search]
      │  │  │       GtkSearchBar
      │  │  └─ GtkScrolledWindow > GtkListView (vault tree)
      │  └─ pages: AdwToolbarView
      │     ├─ top: AdwHeaderBar [back to files] "Pages" [only bookmarks]
      │     ├─ GtkScrolledWindow > GtkGridView (page thumbnails)
      │     └─ bottom: [insert ▾] [paper ▾] [first] [last] [delete]
      └─ content: AdwToolbarView
         ├─ top: AdwHeaderBar [sidebar] [pages] AdwWindowTitle("Lecture 03", "Page 2 of 18")
         │                    [add page] [menu]
         │       AdwTabBar (autohide)
         └─ AdwTabView → per tab: GtkOverlay
            ├─ GtkScrolledWindow > Canvas
            └─ floating pill (bottom centre):
               AdwToggleGroup [favourite presets… | lasso] │ 1–6 colour swatches
               (ink, highlighter) │ 1–5 size slots (any preset) │ [undo] [redo]
```

- **Tools:** the tool group is built in code from the favourite presets (`preset.rs`, in the
  settings; docs/tool-presets.md) plus the lasso: each preset button shows its kind's icon with
  a dot in its current colour; 1–9 pick favourites, L the lasso. A preset is ink, a
  highlighter (drawn over the ink; square tip by default) or an eraser, with its own swatches, size slots,
  opacity (highlighters only; ink is opaque), pressure switch and tip roundness. Tap the active preset again → its options
  (`window/presets.rs`, the same editor as Preferences → Tools, which lists, stars,
  duplicates, deletes and adds presets and picks the stylus's secondary action); the active
  lasso → loop, rectangle, stroke or add space.
  Tap the selected swatch again → palette (`AdwWrapBox`) + an HSV colour wheel drawn from
  gradient nodes (`wheel.rs`); the selected size slot → width slider. How many swatches and
  size slots show is a preference. An eraser preset has size slots (radius in logical px) and
  no swatches; the lasso has neither. Cut, copy,
  paste, delete, change style, arrange, undo and redo are in a context menu (`GtkPopoverMenu`,
  built per opening; cut, copy, delete, change style and arrange only on the selection, Paste greyed out with nothing
  to paste). It opens with any tool, per input: the **mouse** right-clicks anywhere (a compact
  menu below the pointer; left clicks never open it); the **stylus**, or a drawing finger,
  holds still for 500 ms anywhere (the dot, loop or erase it began is dropped) or, with the
  lasso, taps the selection; a **finger** that pans holds still, or taps the selection
  (`touch.rs`). Stylus and finger menus sit just above the press, out from under the hand,
  with 44 px rows, and scroll with a finger when taller than the screen, also from an
  item: a `GtkModelButton` claims a touch on press, which cancels the scrolled window's
  kinetic drag, so each item's click gesture is swapped for one that activates on release;
  palm rejection leaves touches on popovers alone. Opened away from the selection, Paste pastes centred on
  that point. Change style opens a popover at the press with the selection's style: a button per
  non-eraser preset (gives every selected stroke its colour, opacity, width, tip and layer,
  and full pressure if the preset has pressure off), a palette (the highlighter tints when
  the selection is all highlights) and the colour wheel (each stroke keeps its own opacity),
  and beside the wheel a type switch (ink or highlighter: the layer; ink turns opaque, an
  opaque stroke turned highlighter takes the highlighter's default opacity; neither is
  active for a mixed selection) and sliders for width (the first stroke takes the value, the others keep
  their proportion to it), tip and, for an all-highlight selection, opacity. Each pick, and
  each slider once it rests for 250 ms, is one undo step (`restyle_selection`) and redraws
  the selection's texture once; the selection's bounds follow the new widths. Presets, their slots and the secondary action are app settings
  (`~/.config/cera/settings.json`, version 2; version 1's per-tool fields become the three
  default presets). Tool popovers hang off the tool bar box, not the
  `AdwToggleGroup`, which hides children it doesn't know about when they take focus.
- **Pages** (`window/pages.rs`): the Pages toggle next to the sidebar button (or Ctrl+P)
  opens the sidebar on the current note's pages; toggled off or with the back button it
  returns to the files, and hiding the sidebar leaves the pages too. The pages view is a
  `GtkGridView` of page thumbnails: tapping one shows that page, and the selection follows
  the note as it scrolls; below it, insert blank before/after, change paper and delete act
  on the selected page, and two buttons select the first or last page (of the bookmarked
  ones while filtered), which shows it in the note.
  A right click or a hold on a thumbnail selects it and opens its context menu: insert
  before/after, paper, move earlier/later, bookmark (or remove it) and delete; stylus and
  finger menus sit above the press with tall rows, like the canvas's. Thumbnails are whole-page
  render jobs (`Layer::Page`) under their own document id, asked for only for the cells on
  screen and kept in a 16 MiB LRU. Each remembers the page's ink generation (the canvas's
  per-page edit counter) and spec it was drawn from; 400 ms after the last edit the stale
  ones on screen are drawn again, keeping the old picture until then. The sorter follows the
  selected tab, and drops its thumbnails when the pages view or the sidebar is hidden.
- **Bookmarks** (`window/bookmarks.rs`, `window/pages.rs`): Ctrl+D toggles the current
  page's bookmark. In the pages view each thumbnail has a small bookmark toggle in its top
  right corner (faint until set, then accent-coloured), and a header toggle shows only the
  bookmarked pages: a `GtkFilterListModel` over all of them, so grid positions are not page
  numbers and the sorter finds pages by id. Filtered with none set, it shows "No bookmarks".
- **Page selection** (`window/pages.rs`): each thumbnail has a selection toggle in its top
  left corner (an empty ring, then a check on the accent colour). Selected pages are a set
  of page ids in the sorter, apart from the grid's single selection, which stays the current
  page. While any are selected the header shows how many and the bottom bar holds the
  selection tools: select all, invert, select the pages before or after the page selected
  last (if that one was let go: before the last selected page, after the first), and clear.
  They act on the pages shown; pages that stop being shown (deleted, or hidden by the
  bookmark filter) are let go. The selection is dropped with the sorter. The context menu of
  a selected page, while several are, acts on all of them: paper, move earlier/later (each
  page trades places with the unselected neighbour on that side, so a block moves as one and
  pages at that end stay), bookmark (or remove the bookmarks, if all have one), clear the
  selection, and delete (not every page: a note keeps one). Each is one undo step; the menu
  of an unselected page acts on that page only.
- **Import:** drop a PDF on the sidebar/window or use + → Import PDF… → `AdwDialog` with
  `AdwEntryRow` (name), `AdwComboRow` (folder), page range, keep original.
- **No vault open:** `AdwStatusPage` "Open a vault" with "Open folder" / "Create vault" and a
  recent-vaults list.
- **Toasts:** "Imported 18 pages", "Page deleted · Undo".
- **Dark mode:** UI follows `AdwStyleManager`; paper stays white by default (dark paper is a
  later option).
- **Keyboard:** Ctrl+Z / Ctrl+Shift+Z, P / H / E / L for tools, Ctrl+± and Ctrl+0 (fit width),
  F9 sidebar, Ctrl+X/C/V and Delete for the selection, Ctrl+P pages, Ctrl+D bookmark the page;
  Ctrl+? lists them all.
  Keys that text fields also use are bubble-phase window shortcuts, not app accelerators.
- Touch targets ≥ 40 px in the tool bar.

## 10. Memory budget and measurement

Set budgets relative to the measured baseline (GTK + libadwaita alone is a large share):

| Scenario | Target |
| --- | --- |
| Vault open, no note | baseline + ≤ 20 MiB |
| 300-page PDF note, scrolling and zooming | baseline + ≤ 200 MiB RSS |
| GPU textures | ≤ tile budget (128 MiB default) + wet-ink buffer (16 MiB) |

Measured baseline (release build, window open on the "Open a vault" page, idle):

| Machine | RSS | PSS | Notes |
| --- | --- | --- | --- |
| x86 desktop, KDE Wayland, default GSK renderer | 105 MiB | 46 MiB | M0, 2026-10-31; PSS is the fairer number since GTK's shared libs count in full in RSS |
| Galaxy Tab S9 Wi-Fi, Fedora 44, GNOME Wayland, Vulkan (Turnip) | 120 MiB | 59 MiB | M0, 2026-11-01; 43 MiB of the PSS is anonymous memory, much of it likely the Vulkan driver |
| Galaxy Tab S9, vault open, no note | 134 MiB | 77 MiB | M5, 2026-11-01: baseline + 14 MiB (budget + 20) |
| Galaxy Tab S9, two tabs, 324-page note shown | 158 MiB | 100 MiB | M5, 2026-11-01; the hidden tab holds no tiles |
| Galaxy Tab S9, 5 s fast scroll through the 324-page note, Vulkan | 298 MiB | — | 2026-11-01, headless mutter at 2560 × 1600: baseline + 178 MiB (budget + 200); GL 349 MiB, cairo 267 MiB |

How we check:

- `CERA_DEBUG=1` overlay: tile count / bytes per layer, worker queue length, frame time.
- `/proc/<pid>/smaps_rollup` and `heaptrack` on the tablet for each milestone.
- Renderers on the tablet's Mesa driver (`tools/headless/bench.sh`): Vulkan (GTK's default) is
  the documented choice, ~2.5 ms of CPU per frame while scrolling and the least memory of the
  GPU renderers; GL is as fast but uses ~50 MiB more; cairo takes ~11 ms per frame.

## 11. Plan

Milestones, task lists, risks and open questions are in [PLAN.md](PLAN.md).
