# Cera — roadmap

Ideas for after M7, grouped by area. Nothing here is scheduled yet; items move into
[PLAN.md](PLAN.md) as milestones when we pick them up. Architecture lives in
[DESIGN.md](DESIGN.md).

## Adjustments

- [ ] Move the page sorter from its dialog into the sidebar

## Core features

### Canvas and tools

- [ ] Context menu on the canvas (hold or right-click); home for actions that aren't tools,
      such as linking
  - [ ] Layer order for a selection: bring forward, send backward, bring to front, send to back
- [ ] Text tool
- [ ] Shape tool
- [ ] Add-space tool: insert or remove vertical space, standalone or as a selection mode
- [ ] More selection tools: rectangle select, pen (stroke-touch) select, next to the lasso
- [ ] Photos: insert images onto a page

### Toolbar and colours

- [ ] Three size slots in the tool bar, each one editable
- [ ] Colour wheel picker
- [ ] Number of favourite sizes and colours set in preferences

### Linking and navigation

- [ ] Links on ink: attach a link to a selection, pointing at another note's page or a web
      address (a context menu action, not a tool)
- [ ] Bookmark pages

### Vault

- [ ] Folders inside the vault
- [ ] `.md` notes alongside `.cera` notes

## Add-ons

Optional features kept out of the core app so it stays light. Each one is installed and
loaded separately; with none installed, Cera costs nothing extra in RAM or startup time.

- [ ] Add-on system: how add-ons are packaged, discovered, enabled in preferences and given
      access to notes
- [ ] Handwriting recognition: local, multi-language model; model survey in
      [handwriting-recognition.md](handwriting-recognition.md)
  - [ ] Handwriting-aware search (depends on recognition)
- [ ] Handwriting assists: one add-on for writing aids and handwriting cleanup tools
- [ ] Tables and graphs
- [ ] Sticky notes
- [ ] LaTeX
- [ ] Note graph: Obsidian-style map of the vault and its links

## Open questions

- Add-on mechanism: dynamic libraries, separate processes, or optional cargo features built
  into separate Flatpak extensions? This decides what an add-on can touch and what it costs.
- Do `.md` notes get an editor in Cera, or are they shown read-only and opened elsewhere?
- Where links live in the `.cera` format (a new chunk kind, so old readers skip it).
