# Tool presets

## Context

Today the tools are hard-coded. `Tool` is `Pen | Highlighter` (`crates/core/src/model.rs:40`).
`ToolKind` is `Pen | Highlighter | Eraser | Lasso` (`crates/app/src/canvas/input.rs:62`).
Each tool has its own colour swatches and size slots, stored as separate fields in `Settings`
(`crates/app/src/settings.rs`). Pressure is one global switch, and the eraser only removes
whole strokes.

The goal is to let users build any pen-like tool they want. The app ships three presets
(pen, highlighter, eraser), and the eraser is the stylus's secondary action. Users can
favourite any preset to the tool bar, remove the defaults from it, and pick any preset as the
secondary action.

Options per preset:
- base size
- opacity
- pressure
- tip roundness (square to round)
- highlighter (always drawn over the ink)
- line pattern (solid, dashed or dotted, with a spacing slider)
- straight line with angle snapping (later, in the "Handwriting assists" add-on)
- eraser (whole strokes or partial)

Turning on the eraser greys out the options it doesn't use.

It lands in two milestones. Each one ends with a
"test on the tablet" list.

- **M8 — Presets:** the style model and file format, layers, tip roundness, opacity, presets
  in settings, the tool bar favourites, the preset popover, the Tools preferences page and the
  secondary preset.
- **M9 — Eraser modes and patterns:** the partial eraser and dashed and dotted lines.

Straight lines and angle snapping are not in either milestone. They go to the "Handwriting
assists" add-on in ROADMAP.md, because they change how you draw, not how a stroke is stored
(a straight line is just a two-point stroke). That add-on waits for the add-on system.

## Decisions (from our Q&A)

- **Swatches and sizes:** each preset has its own colour swatches and size slots, like
  pen and highlighter today.
- **Secondary action:** one preset (or the lasso) is the secondary action, for both the eraser
  end and the barrel button. The default is the eraser preset. This replaces the current
  "barrel = eraser or lasso" setting.
- **Highlighter and z-order:** a preset is ink, a highlighter or an eraser. A highlighter is
  drawn above all ink (changed after M8 landed: the first build had a "draw over ink" option,
  off by default; it went, and over the ink became the only behaviour). The default
  highlighter has a square tip.
  - Ink keeps plain drawing order. Each tile draws three passes: highlights under (strokes
    from before presets, whose highlighters were drawn under the ink), then ink, then
    highlights over.
- **Tip roundness:** one slider. 0 is a square nib aligned to the page axes, 1 is round, and
  values in between give a rounded square. There is no nib angle.
- **Partial eraser:** cuts out the points inside the eraser circle and splits the stroke into
  pieces, with the cut ends placed on the circle. Each piece is an ordinary stroke. One undo
  step per drag.
- **Eraser scope:** erases everything; no filters.
- **Pattern:** solid, dashed or dotted, with one spacing slider in multiples of the stroke
  width. The spacing sets the gap, and also the dash length for dashed lines. A dot is a stamp
  of the tip shape.
- **Opacity:** highlighters only; ink is always opaque and its opacity row is greyed out
  (changed after M8 landed). A stroke never darkens itself (it is one fill). Separate strokes
  do stack.
- **Straight line (add-on, later):** a preset option that draws from pen-down to the current
  point. Snapping is a separate option with a configurable angle step (15°, 30°, 45° or 90°),
  with a ±5° tolerance. These go on the ROADMAP under "Handwriting assists"; this plan only
  records the design.
- **Pressure:** a toggle per preset. An eraser preset keeps only its size slots and its mode;
  everything else is greyed out.

## Defaults

- **The lasso stays a fixed tool** in the tool bar. It is not a preset.
- **Keyboard:** 1–9 pick the tool bar favourites in order, and L picks the lasso. P, H and E
  go away, since presets can be renamed or deleted.
- **Preset popover:**
  - Tap the active preset to open its popover. It holds every option, the swatches and size
    slots, and a "Manage presets…" button that opens the Tools page in Preferences.
  - The pressure toggle that today sits on a tap of the active pen moves into this popover.

## M8 — Presets

### Core (`crates/core`)
- In `model.rs`, replace `Tool` on `Stroke` with a small `StrokeStyle`:
  - `layer`: `Under | Ink | Over`
  - `roundness: u8` (0–255)
  - `pattern`: `Solid | Dash | Dot`
  - `spacing: u8` (in 1/16 width units)

  It is `Copy` and about 4 bytes, so strokes stay `Arc`-shared and barely grow. Opacity stays
  in `Rgba8::a`. Pressure needs no stroke field: with pressure off, capture records
  pressure 1.0.
- In `codec.rs`, bump the ink `ENCODING` to 2 and write the style after the colour.
  - Keep the v1 reader: `Pen` becomes `Ink`, round, solid. `Highlighter` becomes `Under`,
    round, solid, which matches today's round caps.
  - Add golden-bytes tests for v1 and v2.
  - Update DESIGN.md §"Stroke encoding".
- Update the users of `stroke.tool()`. In `lasso.rs`, recolouring keeps each stroke's alpha,
  and reorder groups by layer.

### Render (`crates/render/src/ink.rs`)
- `draw_ink` draws the three layer passes in a fixed order (Under, Ink, Over), with one
  filter per pass as today.
- Outlines:
  - Roundness 1 keeps the current `add_pen_path` fast path.
  - Roundness below 1 builds a rounded-square nib, sized by pressure. Each segment becomes
    the convex hull of its two end nibs, and all hulls go in one path filled with the winding
    rule. That gives one shape, so opacity stays flat.
  - The old highlighter `cr.stroke()` path stays for decoded round, pressureless strokes.
- `hits` uses the nib's half-diagonal as its reach when roundness is below 1.
- `crates/app/src/canvas/wet.rs` mirrors this for the live stroke.
- Benchmark with `cargo bench -p cera-render`: the square-nib path against the round one,
  with frame times reported.

### App (`crates/app`)
- In `settings.rs`, add a `Preset` struct:
  - `id`, `name`
  - `role`: `Ink | Highlighter | Eraser { mode }`
  - `colors`, `widths`, `width`, `opacity`, `pressure`, `roundness`
  - `pattern`, `spacing` (stored now, used in M9)

  Settings use `#[serde(default)]`, so the add-on can add its straight-line fields later
  without a migration.

  `Settings` gets `presets: Vec<Preset>`, `favorites: Vec<id>`, `active` and `secondary`.
- Bump the settings `version`. A migration turns the old per-tool fields into the three
  default presets, with the old highlighter colours' alpha becoming `opacity`.
- `ToolKind` becomes `Preset(id) | Lasso`. `ToolState`'s width and colour getters read the
  active preset.
- `window/toolbar.rs`:
  - The toggle group is built from `favorites` plus the lasso.
  - Each button shows a role icon tinted with the preset's colour.
  - The swatch and size rows come from the active preset.
  - Tapping the active preset opens its popover, a new `window/preset_editor.rs` built from
    `AdwPreferencesGroup` rows. The rows that don't apply are made insensitive (greyed out).
- `window/prefs.rs` gets a new "Tools" page:
  - It lists every preset, with a favourite star, a "secondary action" radio, edit (the same
    editor as the popover), duplicate and delete.
  - It also has "Add preset" and "Restore default presets".
  - The barrel option moves here.
- `canvas/input.rs`: the eraser end and barrel use `secondary` instead of the hard-coded
  eraser.

## M9 — Eraser modes and patterns

- **Core:**
  - `split_stroke(stroke, centre, radius) -> Vec<Stroke>` in a new `crates/core/src/erase.rs`.
    It interpolates the cut points onto the circle and drops pieces that are too short, and
    is covered by proptests.
  - The partial eraser gathers one `ReplaceStrokes` per drag (`undo.rs`).
- **Patterns** (render): walk the centreline by arc length.
  - Dash: an outline for each dash piece, all in one path.
  - Dot: tip stamps at each interval.
  - Clip to the tile `area` as today. Hit-testing treats patterned strokes as solid.
- **Docs:**
  - Remove "Partial eraser" from PLAN.md "Later" and update DESIGN.md §6 (eraser) and the
    tool bar section.
  - In ROADMAP.md, add straight lines with configurable angle snapping (a preset option) to
    "Handwriting assists".

## Efficiency notes

- There are no new caches or dependencies.
- The style adds about 4 bytes per stroke in memory and 4 bytes per stroke on disk.
- The third render pass is a filter over the same slice, with nothing allocated.
- The square-nib outline costs more path segments. It is benchmarked before landing, and it is
  only used when roundness is below 1.
- The partial eraser replaces strokes in place, so tiles are invalidated only in the dirty
  area, as with the whole-stroke eraser.

## Verification

- `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace` and a build must all pass.
  - New tests: codec v1→v2 golden bytes, settings migration and `split_stroke` proptests.
- `cargo bench -p cera-render`: square tip against round.
- `cargo run -p cera-cli -- png` on an old v1 note gives output identical to before.
- Headless: `tools/headless/run.sh` draws with the mouse using each default preset, and with a
  square-tip highlighter over the ink. The screenshots are compared.
- Test on the tablet:
  - M8: favourite and unfavourite presets, create a square highlighter and write over ink,
    pick a different secondary preset for the barrel, and open old notes.
  - M9: partial-erase through pressure strokes, draw dashed and dotted lines, and check undo.
