# Cera — roadmap

Ideas for after M7, grouped by area. Nothing here is scheduled yet; items move into
[PLAN.md](PLAN.md) as milestones when we pick them up. Architecture lives in
[DESIGN.md](DESIGN.md).

## Difficulty

Each item carries a rough difficulty, judged against the code as of 2026.11.1:

- **D1** a day or two, no format change
- **D2** a few days: UI plumbing, input handling or a manifest version bump
- **D3** a week or more: a new concept in the model, a new chunk kind or a new dependency
- **D4** several weeks: architecture work
- **D5** research-grade

A page holds only strokes today. Text, photos, sticky notes, tables and LaTeX all need page
elements beyond strokes (format bump, hit-testing, selection, undo, export): build that
element model once, with photos as its first and simplest user, then the rest get cheaper.
Every add-on waits on the add-on system.

## Adjustments

- [ ] Move the page sorter from its dialog into the sidebar — D2

## Core features

### Canvas and tools

- [ ] Context menu on the canvas (hold or right-click); home for actions that aren't tools,
      such as linking — D2 (a hold must not fight drawing). Started: with any tool it opens
      from a mouse right-click, a 500 ms stylus or finger hold, or a tap on the selection
  - [x] Layer order for a selection: bring forward, send backward, bring to front, send to
        back — D1 (in the context menu; Ctrl+] / Ctrl+[)
- [ ] Text tool — D4 (element model, Pango in the render worker, on-screen keyboard)
- [ ] Shape tool — D3 if shapes are saved as plain strokes, D4 as editable objects
- [ ] Add-space tool: insert or remove vertical space, standalone or as a selection mode — D2
      (D3 if ink pushed off the page spills onto the next one)
- [ ] More selection tools: rectangle select, pen (stroke-touch) select, next to the lasso —
      D1 rectangle, D2 pen
  - [x] Rectangle select (a lasso mode: tap the active lasso)
- [ ] Photos: insert images onto a page — D4 (first page element; image asset kind, downscale
      on import)
- [ ] Tool presets: user-made pen-like tools (size, opacity, pressure, tip roundness,
      highlighter under or over ink, dashed and dotted lines, whole or partial eraser),
      favourites in the tool bar, any preset as the stylus's secondary action — D3 (stroke
      style in the ink encoding); plan in [tool-presets.md](tool-presets.md)

### Toolbar and colours

- [x] Three size slots in the tool bar, each one editable — D1
- [x] Colour wheel picker — D1
- [x] Number of favourite sizes and colours set in preferences — D1 (with the size slots)

### Linking and navigation

- [ ] Links on ink: attach a link to a selection, pointing at another note's page or a web
      address (a context menu action, not a tool) — D3 (strokes have no ids to anchor to;
      links must survive renames and moves)
- [ ] Bookmark pages — D2

### Vault

- [ ] Folders inside the vault — see open questions
- [ ] `.md` notes alongside `.cera` notes — D3 shown read-only, D4 with an editor

## Add-ons

Optional features kept out of the core app so it stays light. Each one is installed and
loaded separately; with none installed, Cera costs nothing extra in RAM or startup time.

- [ ] Add-on system: how add-ons are packaged, discovered, enabled in preferences and given
      access to notes — D4
- [ ] Handwriting recognition: local, multi-language model; model survey in
      [handwriting-recognition.md](handwriting-recognition.md) — D5
  - [ ] Handwriting-aware search (depends on recognition) — D3 once recognition works
- [ ] Handwriting assists: one add-on for writing aids and handwriting cleanup tools — D2 for
      smoothing and straightening, D4 for real cleanup
  - [ ] Straight lines as a preset option, with snapping to a configurable angle step —
        D2 (design in [tool-presets.md](tool-presets.md))
- [ ] Tables and graphs — D4
- [ ] Sticky notes — D3 once the element model exists
- [ ] LaTeX — D4 (embedded typesetter or an installed TeX)
- [ ] Note graph: Obsidian-style map of the vault and its links — D3 once links exist

## Open questions

- Add-on mechanism: dynamic libraries, separate processes, or optional cargo features built
  into separate Flatpak extensions? This decides what an add-on can touch and what it costs.
- Do `.md` notes get an editor in Cera, or are they shown read-only and opened elsewhere?
- Where links live in the `.cera` format (a new chunk kind, so old readers skip it).
- Folders inside the vault: the vault has had folders since M5 (new folder, rename,
  drag-and-drop move). Is something missing, or did this mean sections inside a note?
