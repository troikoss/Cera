# CLAUDE.md

Cera: a stylus-first handwritten-notes app in Rust + GTK4 + libadwaita.
PDFs are imported into a native `.cera` file and their pages become page backgrounds.
Notes live in an Obsidian-style vault folder.

- Design and reasoning: [docs/DESIGN.md](docs/DESIGN.md)
- Milestones, task lists, risks: [docs/PLAN.md](docs/PLAN.md). Tick boxes there as work lands
  and update its **Status** line.

## The one rule: efficiency

The target is an ARM tablet with ~4 GB RAM and a Mali GPU sharing system memory. Rnote is too
heavy there; that's why this app exists. Weigh every change against RAM and GPU cost:

- Cache per screen tile, never per stroke or per object. Caches have byte budgets.
- Load lazily (page ink near the viewport only); release caches for inactive tabs.
- Never keep a second copy of big data: PDFs stay mmap'd, strokes are `Arc`-shared.
- No retained cairo surfaces; rendered tiles become `gdk::Texture`s that are reused across frames.
- The snapshot function only appends cached textures and cheap nodes; no rasterising in it.
- Don't add a dependency without a reason; no tokio, WebKit or librsvg.
- When a change could affect memory or frame time, say how it was measured.

## Workflow

Claude implements one milestone at a time; the user reviews the code and tests on the tablet.
Keep changes reviewable: small modules, focused commits, and end each milestone with a short
"test on the tablet" list. Don't start the next milestone until the user gives feedback.

## Layout (planned, from M0)

```
crates/core     model, .cera format, geometry, stroke outlines, undo — no GTK, fully unit-tested
crates/render   cairo + Poppler tile rasteriser and render worker thread — no widgets
crates/app      GTK/libadwaita UI: window, vault sidebar, canvas widget, input
crates/cli      cera-cli: import / info / compact (testing aid)
docs/           DESIGN.md, PLAN.md
```

Keep GTK types out of `core`. Poppler objects aren't `Send`: each render worker opens its own
`poppler::Document`.

## Commands

System packages (Ubuntu 26.04):

```bash
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev libpoppler-glib-dev
```

```bash
cargo run -p cera              # run the app
cargo test --workspace         # tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
RUST_LOG=cera=debug CERA_DEBUG=1 cargo run -p cera       # logs + memory/frame overlay
cargo run -p cera --release --example stylus             # M1 stylus spike (see its header)
GSK_RENDERER=gl cargo run -p cera                        # compare renderers (gl, vulkan, cairo)
```

All four of fmt, clippy, test and a successful build must pass before a commit.

## Stack

`gtk4` 0.11 (feature `v4_18`), `libadwaita` 0.9 as `adw` (feature `v1_7`), `poppler-rs` 0.26,
cairo-rs/glib/gio 0.22, `rstar`, `memmap2`, `postcard`, `async-channel`, `tracing`.
Licence GPL-3.0-or-later (required by Poppler).

## Conventions

- Rust edition 2024; `thiserror` for library errors in `core`/`render`, `anyhow` in `app`/`cli`.
- GTK widgets as subclasses with composite templates (`.ui` XML in `crates/app/data/`),
  compiled into a GResource by `build.rs`.
- Page coordinates are PDF points (1/72 in) everywhere in the model.
- The `.cera` format is versioned: bump the version and keep a reader for old files when
  changing the encoding; unknown chunk kinds must be skipped, not rejected.
- UI text in sentence case, following the GNOME HIG.
