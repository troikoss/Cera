# CLAUDE.md

Cera: a stylus-first handwritten-notes app in Rust + GTK4 + libadwaita.
PDFs are imported into a native `.cera` file and their pages become page backgrounds.
Notes live in an Obsidian-style vault folder.

- Design and reasoning: [docs/DESIGN.md](docs/DESIGN.md)
- Milestones, task lists, risks: [docs/PLAN.md](docs/PLAN.md). Tick boxes there as work lands
  and update its **Status** line.
- Feature ideas and add-ons, not yet scheduled: [docs/ROADMAP.md](docs/ROADMAP.md)

## The one rule: efficiency

The target is a Galaxy Tab S9 (Fedora 44, 8 GB RAM, Adreno 740 sharing system memory, 2560×1600
screen at 2× scale). It has room, but the app should stay far lighter than Rnote; that's why it
exists. Budgets are in DESIGN.md §10. Weigh every change against RAM and GPU cost:

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

## Layout

```
crates/core     model, .cera format, geometry, stroke outlines, undo — no GTK, fully unit-tested
crates/render   cairo + Poppler tile rasteriser and render worker thread — no widgets
crates/app      GTK/libadwaita UI: window, vault sidebar, canvas widget, input
crates/cli      cera-cli: import / info / compact / export / png (testing aid)
docs/           DESIGN.md, PLAN.md
build-aux/      install.sh, Flatpak manifest (desktop file, metainfo, icons live in crates/app/data)
tools/headless/ scripted real-input checks and renderer benchmark in a headless mutter
```

Keep GTK types out of `core`. Poppler objects aren't `Send`: each render worker opens its own
`poppler::Document`.

## Commands

System packages (Ubuntu 26.04 desktop; Fedora 44 on the tablet):

```bash
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev libpoppler-glib-dev
```

```bash
sudo dnf install gcc pkgconf-pkg-config gtk4-devel libadwaita-devel poppler-glib-devel
```

```bash
cargo run -p cera              # run the app
cargo test --workspace         # tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
RUST_LOG=cera=debug CERA_DEBUG=1 cargo run -p cera       # logs + memory/frame overlay
cargo run -p cera --release --example stylus             # M1 stylus spike (see its header)
CERA_DEBUG_DEMO=1 cargo run -p cera -- file.cera          # scripted draw/erase/undo
cargo bench -p cera-render                               # ink geometry benchmarks
GSK_RENDERER=gl cargo run -p cera                        # compare renderers (gl, vulkan, cairo)
cargo run -p cera-cli -- export note.cera out.pdf        # also: png, import, info, compact
tools/rnote2cera.py note.rnote [--pdf book.pdf]          # convert Rnote notes (see its header)
cargo build --release -p cera && build-aux/install.sh    # install into ~/.local
flatpak-builder --user --install --force-clean build-dir build-aux/io.github.troikoss.Cera.yml
build-aux/release-linux.sh                               # release tarballs: x86_64, i686, aarch64
```

Versions are the release date, `YYYY.M.D` (semver allows no leading zeros); tags are `v<version>`.

Scripted checks with real input, on the tablet over SSH: `tools/headless/session.sh start`
runs Cera in a private headless mutter (own D-Bus and Wayland display, real GPU, nothing on
screen); `tools/headless/run.sh '<steps>'` injects pointer, keys and touch through mutter's
RemoteDesktop API and saves screenshots (see `drive.py`); `tools/headless/bench.sh note.cera`
compares renderers. The stylus can't be injected. With `CERA_DEBUG=1` the app also has
`app.win` (run any window action over D-Bus) and `app.screenshot`.

All four of fmt, clippy, test and a successful build must pass before a commit.

## Stack

`gtk4` 0.11 (feature `v4_18`), `libadwaita` 0.9 as `adw` (feature `v1_7`), `poppler-rs` 0.26,
cairo-rs/glib/gio 0.22, `memmap2`, `postcard`, `async-channel`, `tracing`; `criterion` and
`proptest` for benches and tests. (`rstar` is deferred: linear stroke scans are fast enough.)
Licence GPL-3.0-or-later (required by Poppler).

## Conventions

- Rust edition 2024; `thiserror` for library errors in `core`/`render`, `anyhow` in `app`/`cli`.
- GTK widgets as subclasses with composite templates (`.ui` XML in `crates/app/data/`),
  compiled into a GResource by `build.rs`.
- Page coordinates are PDF points (1/72 in) everywhere in the model.
- The `.cera` format is versioned: bump the version and keep a reader for old files when
  changing the encoding; unknown chunk kinds must be skipped, not rejected.
- UI text in sentence case, following the GNOME HIG.
