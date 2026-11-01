# Cera

Stylus-first handwritten notes for Linux tablets, in Rust with GTK 4 and libadwaita. Named
after *cera*, the wax of Roman writing tablets.

- Pressure-sensitive pen, highlighter, stroke eraser and lasso (move, recolour, cut, copy,
  paste between notes), with palm rejection and pinch zoom
- PDFs are imported into notes and their pages become backgrounds you write on; export back
  to a vector PDF, or a page to PNG
- Notes are `.cera` files in an Obsidian-style vault folder: a sidebar tree, tabs, search
- Ruled, grid and dotted paper; a page sorter to reorder, insert and delete pages
- Light: tiled rendering with a fixed cache budget, lazy loading, append-only crash-safe
  autosave

Design: [docs/DESIGN.md](docs/DESIGN.md). Plan and status: [docs/PLAN.md](docs/PLAN.md).

## Build and run

Needs Rust (edition 2024), GTK ≥ 4.18, libadwaita ≥ 1.7 and poppler-glib.

```bash
sudo dnf install gcc pkgconf-pkg-config gtk4-devel libadwaita-devel poppler-glib-devel
```

```bash
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev libpoppler-glib-dev
```

```bash
cargo run --release -p cera
```

## Install

For your user (binary, desktop entry, icon, `.cera` file type), into `~/.local`:

```bash
cargo build --release -p cera && build-aux/install.sh
```

Or as a Flatpak (x86_64 or aarch64):

```bash
flatpak-builder --user --install --force-clean build-dir build-aux/io.github.troikoss.Cera.yml
```

## Testing aids

```bash
cargo test --workspace
cargo run -p cera-cli -- info note.cera          # also: import, compact, export, png, scribble
CERA_DEBUG=1 cargo run -p cera                   # memory and frame-time overlay
```

## Licence

GPL-3.0-or-later (Poppler is GPL). See [COPYING](COPYING).
