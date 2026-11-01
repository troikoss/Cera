// SPDX-License-Identifier: GPL-3.0-or-later
//! The note canvas: pages stacked vertically inside a `GtkScrolledWindow`, drawn from cached
//! tile textures (DESIGN.md §5). Implements `GtkScrollable`.
//!
//! - `mod.rs`: the widget, scrolling and zoom.
//! - `tiles.rs`: asking the render workers for PDF and ink tiles, and drawing a frame.
//! - `input.rs`: stylus, mouse and eraser; strokes go into the note through its `History`.
//! - `wet.rs`: the stroke being drawn, until its finished tiles arrive.
//! - `lasso.rs`: selecting, moving, recolouring, cutting and pasting strokes.
//!
//! `snapshot` only appends cached textures and colour nodes.

mod cache;
mod input;
mod lasso;
mod layout;
mod tiles;
mod wet;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

use cera_core::{Changes, Document, PageId, PageSpec, Rect};
use cera_render::tile::{DocId, Grid, Layer, Scale};
use cera_render::worker::{Renderer, TileResult};
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib};

use self::cache::TileCache;
pub use self::input::{
    ERASER_WIDTH_RANGE, ERASER_WIDTHS, HIGHLIGHTER_ALPHA, HIGHLIGHTER_COLORS,
    HIGHLIGHTER_WIDTH_RANGE, HIGHLIGHTER_WIDTHS, MAX_COLOR_SLOTS, MAX_SIZE_SLOTS, PEN_COLORS,
    PEN_WIDTH_RANGE, PEN_WIDTHS, ToolKind, Tools,
};
pub use self::lasso::{HOLD_SLOP_PX, HOLD_TIME, InputSource, has_clipboard};
use self::layout::{Layout, View};
use self::wet::WetStroke;
use crate::note::SharedNote;

/// Default tile cache budget (DESIGN.md §5; a preference). It counts the CPU copy of each
/// texture; GSK keeps GPU copies of recently drawn tiles on top.
const TILE_BUDGET: usize = 128 << 20;
/// Request new tiles this long after the zoom stops changing.
const SETTLE: Duration = Duration::from_millis(150);
/// Logical pixels per point.
const MIN_ZOOM: f64 = 0.1;
const MAX_ZOOM: f64 = 8.0;

#[cfg(target_endian = "little")]
const PIXEL_FORMAT: gdk::MemoryFormat = gdk::MemoryFormat::B8g8r8a8Premultiplied;
#[cfg(target_endian = "big")]
const PIXEL_FORMAT: gdk::MemoryFormat = gdk::MemoryFormat::A8r8g8b8Premultiplied;

/// Page ids, specs and layout of the open note.
struct CanvasDoc {
    id: DocId,
    pages: Vec<(PageId, PageSpec)>,
    layout: Layout,
}

impl CanvasDoc {
    fn new(id: DocId, doc: &Document) -> Self {
        let pages: Vec<_> = doc.pages().iter().map(|p| (p.id(), *p.spec())).collect();
        let layout = Layout::new(pages.iter().map(|(_, s)| (s.width, s.height)));
        Self { id, pages, layout }
    }

    fn page_index(&self, id: PageId) -> Option<usize> {
        self.pages.iter().position(|(p, _)| *p == id)
    }
}

/// The visible ink tiles of a page that should exist at a scale (those with strokes).
type ExpectedInk = (Scale, Vec<(u16, u16)>);

#[derive(Clone, Copy)]
struct Pinch {
    zoom: f64,
    anchor_pt: (f64, f64),
}

#[derive(Default)]
struct FrameStats {
    frames: u32,
    total_us: i64,
    max_us: i64,
    paint_start: i64,
}

mod imp {
    use super::*;

    pub struct Canvas {
        pub(super) hadjustment: RefCell<Option<(gtk::Adjustment, glib::SignalHandlerId)>>,
        pub(super) vadjustment: RefCell<Option<(gtk::Adjustment, glib::SignalHandlerId)>>,
        pub(super) hscroll_policy: Cell<gtk::ScrollablePolicy>,
        pub(super) vscroll_policy: Cell<gtk::ScrollablePolicy>,
        pub(super) note: RefCell<Option<SharedNote>>,
        pub(super) doc: RefCell<Option<CanvasDoc>>,
        pub(super) renderer: OnceCell<Rc<Renderer>>,
        /// Only the visible tab renders; hidden ones hold no tiles.
        pub(super) visible: Cell<bool>,
        /// Page to show once the canvas has a size.
        pub(super) pending_page: Cell<Option<usize>>,
        pub(super) cache: RefCell<TileCache<gdk::Texture>>,
        pub(super) zoom: Cell<f64>,
        /// Zoom the tiles are rendered for; lags `zoom` while it changes. 0 until laid out.
        pub(super) settled_zoom: Cell<f64>,
        pub(super) fit_width: Cell<bool>,
        pub(super) settle: RefCell<Option<glib::SourceId>>,
        /// Pages whose PDF background or ink can't be read; not asked for again.
        pub(super) failed: RefCell<HashSet<(DocId, PageId, Layer)>>,
        /// Ink generation per page, bumped by every edit; tiles drawn for an older one are stale.
        pub(super) ink_gen: RefCell<HashMap<PageId, u64>>,
        /// Visible ink tiles (at a scale) that should exist, so a page is drawn at one scale.
        pub(super) ink_expected: RefCell<HashMap<PageId, ExpectedInk>>,
        pub(super) active: RefCell<Option<input::Active>>,
        /// Finished strokes still shown wet until their tiles arrive.
        pub(super) wet: RefCell<Vec<WetStroke>>,
        pub(super) lasso: RefCell<Option<lasso::LassoPath>>,
        pub(super) selection: RefCell<Option<lasso::Selection>>,
        /// What pressed last (stylus, mouse or finger), for the context menu.
        pub(super) input_source: Cell<lasso::InputSource>,
        /// A stylus or finger press that may turn into a hold.
        pub(super) hold: RefCell<Option<lasso::Hold>>,
        /// Where the last press began (widget coordinates); the context menu opens there.
        pub(super) press: Cell<(f64, f64)>,
        pub(super) ghosts: RefCell<Vec<lasso::Ghost>>,
        pub(super) erasing: Cell<bool>,
        /// The stroke this pen contact drew; a barrel press mid-stroke erases around it.
        pub(super) drawn: RefCell<Option<std::sync::Arc<cera_core::Stroke>>>,
        pub(super) eraser_cursor: Cell<Option<(f64, f64)>>,
        pub(super) tools: RefCell<Rc<Tools>>,
        pub(super) current_page: Cell<u32>,
        pub(super) pinch: Cell<Option<Pinch>>,
        pub(super) pointer: Cell<(f64, f64)>,
        pub(super) stats: RefCell<FrameStats>,
        pub(super) memory_monitor: OnceCell<gio::MemoryMonitor>,
    }

    impl Default for Canvas {
        fn default() -> Self {
            Self {
                hadjustment: RefCell::default(),
                vadjustment: RefCell::default(),
                hscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                vscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                note: RefCell::default(),
                doc: RefCell::default(),
                renderer: OnceCell::new(),
                visible: Cell::new(true),
                pending_page: Cell::new(None),
                cache: RefCell::new(TileCache::new(TILE_BUDGET)),
                zoom: Cell::new(1.0),
                settled_zoom: Cell::new(0.0),
                fit_width: Cell::new(true),
                settle: RefCell::default(),
                failed: RefCell::default(),
                ink_gen: RefCell::default(),
                ink_expected: RefCell::default(),
                active: RefCell::default(),
                wet: RefCell::default(),
                lasso: RefCell::default(),
                selection: RefCell::default(),
                input_source: Cell::default(),
                hold: RefCell::default(),
                press: Cell::default(),
                ghosts: RefCell::default(),
                erasing: Cell::new(false),
                drawn: RefCell::default(),
                eraser_cursor: Cell::new(None),
                tools: RefCell::default(),
                current_page: Cell::new(0),
                pinch: Cell::new(None),
                pointer: Cell::new((0.0, 0.0)),
                stats: RefCell::default(),
                memory_monitor: OnceCell::new(),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Canvas {
        const NAME: &'static str = "CeraCanvas";
        type Type = super::Canvas;
        type ParentType = gtk::Widget;
        type Interfaces = (gtk::Scrollable,);
    }

    impl ObjectImpl for Canvas {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPS: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
            PROPS.get_or_init(|| {
                vec![
                    glib::ParamSpecOverride::for_interface::<gtk::Scrollable>("hadjustment"),
                    glib::ParamSpecOverride::for_interface::<gtk::Scrollable>("vadjustment"),
                    glib::ParamSpecOverride::for_interface::<gtk::Scrollable>("hscroll-policy"),
                    glib::ParamSpecOverride::for_interface::<gtk::Scrollable>("vscroll-policy"),
                    glib::ParamSpecUInt::builder("current-page")
                        .read_only()
                        .build(),
                    glib::ParamSpecUInt::builder("page-count")
                        .read_only()
                        .build(),
                ]
            })
        }

        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: OnceLock<Vec<glib::subclass::Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    // After every edit, undo or redo: for autosave and the undo buttons.
                    glib::subclass::Signal::builder("changed").build(),
                    // A lasso selection was made or ended.
                    glib::subclass::Signal::builder("selection-changed").build(),
                    // Show the lasso's actions (a tap on the selection, a hold, a right
                    // click).
                    // Params: x, y (the press), `InputSource` as u32, and whether it is away
                    // from the selection (where Paste would put the strokes).
                    glib::subclass::Signal::builder("context-menu")
                        .param_types([
                            f64::static_type(),
                            f64::static_type(),
                            u32::static_type(),
                            bool::static_type(),
                        ])
                        .build(),
                ]
            })
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            let obj = self.obj();
            match pspec.name() {
                "hadjustment" => {
                    obj.set_adjustment(gtk::Orientation::Horizontal, value.get().unwrap())
                }
                "vadjustment" => {
                    obj.set_adjustment(gtk::Orientation::Vertical, value.get().unwrap())
                }
                "hscroll-policy" => self.hscroll_policy.set(value.get().unwrap()),
                "vscroll-policy" => self.vscroll_policy.set(value.get().unwrap()),
                _ => unreachable!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "hadjustment" => self
                    .hadjustment
                    .borrow()
                    .as_ref()
                    .map(|(a, _)| a.clone())
                    .to_value(),
                "vadjustment" => self
                    .vadjustment
                    .borrow()
                    .as_ref()
                    .map(|(a, _)| a.clone())
                    .to_value(),
                "hscroll-policy" => self.hscroll_policy.get().to_value(),
                "vscroll-policy" => self.vscroll_policy.get().to_value(),
                "current-page" => self.current_page.get().to_value(),
                "page-count" => self.obj().page_count().to_value(),
                _ => unreachable!(),
            }
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.add_css_class("note-canvas");
            obj.set_overflow(gtk::Overflow::Hidden);
            obj.setup_zoom_input();
            obj.setup_drawing_input();

            let monitor = gio::MemoryMonitor::dup_default();
            let weak = obj.downgrade();
            monitor.connect_low_memory_warning(move |_, level| {
                if let Some(canvas) = weak.upgrade() {
                    canvas.imp().cache.borrow_mut().retain_current_frame();
                    tracing::info!(?level, "low memory: dropped off-screen tiles");
                }
            });
            let _ = self.memory_monitor.set(monitor);
        }
    }

    impl WidgetImpl for Canvas {
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            let obj = self.obj();
            obj.fit_if_needed();
            match self.pending_page.get().filter(|_| width > 0) {
                Some(page) => {
                    self.pending_page.set(None);
                    obj.show_page(page);
                }
                None => obj.configure_adjustments(None),
            }
            obj.request_tiles();
        }

        fn realize(&self) {
            self.parent_realize();
            if crate::debug_enabled() {
                self.obj().track_frame_times();
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            self.obj().draw(snapshot);
        }
    }

    impl ScrollableImpl for Canvas {}
}

glib::wrapper! {
    pub struct Canvas(ObjectSubclass<imp::Canvas>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Scrollable;
}

impl Default for Canvas {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl Canvas {
    /// Gives the canvas the window's render threads; call once, before `set_note`.
    pub fn set_renderer(&self, renderer: Rc<Renderer>) {
        let _ = self.imp().renderer.set(renderer);
    }

    /// Shares the window's tool settings; call before drawing.
    pub fn set_tools(&self, tools: Rc<Tools>) {
        *self.imp().tools.borrow_mut() = tools;
    }

    fn tools(&self) -> Rc<Tools> {
        self.imp().tools.borrow().clone()
    }

    pub fn note(&self) -> Option<SharedNote> {
        self.imp().note.borrow().clone()
    }

    pub fn doc_id(&self) -> Option<DocId> {
        self.imp().doc.borrow().as_ref().map(|d| d.id)
    }

    /// A hidden tab drops its tiles and stops rendering; showing it asks for them again.
    pub fn set_active(&self, active: bool) {
        let imp = self.imp();
        if imp.visible.replace(active) == active {
            return;
        }
        if active {
            self.request_tiles();
            self.queue_draw();
        } else if let Some(id) = self.doc_id() {
            self.renderer().set_jobs(id, Vec::new());
            imp.cache.borrow_mut().remove_doc(id);
            imp.ink_expected.borrow_mut().clear();
            imp.wet.borrow_mut().clear();
            imp.ghosts.borrow_mut().clear();
        }
    }

    /// Scrolls so `page` (0-based) is at the top; waits for the first layout if needed.
    pub fn show_page(&self, page: usize) {
        let imp = self.imp();
        if self.width() == 0 || imp.settled_zoom.get() == 0.0 {
            imp.pending_page.set(Some(page));
            return;
        }
        let y = {
            let doc = imp.doc.borrow();
            let Some(doc) = doc.as_ref() else { return };
            let page = page.min(doc.layout.len().saturating_sub(1));
            (doc.layout.page_rect(page).y - layout::GAP_PT) * imp.zoom.get()
        };
        let x = self.scroll().0;
        self.configure_adjustments(Some((x, y.max(0.0))));
        self.update_current_page();
    }

    /// Shows `note` (or nothing), fitted to the width and scrolled to the top.
    pub fn set_note(&self, note: Option<SharedNote>) {
        let imp = self.imp();
        let renderer = self.renderer();
        if let Some(old) = imp.doc.take() {
            imp.cache.borrow_mut().remove_doc(old.id);
            imp.failed.borrow_mut().retain(|(d, ..)| *d != old.id);
            renderer.close_doc(old.id);
        }
        imp.active.take();
        imp.wet.borrow_mut().clear();
        imp.lasso.take();
        imp.selection.take();
        imp.ghosts.borrow_mut().clear();
        imp.ink_gen.borrow_mut().clear();
        imp.ink_expected.borrow_mut().clear();
        if let Some(note) = &note {
            let note = note.borrow();
            let file = &note.file;
            renderer.open_doc(
                note.id,
                file.assets()
                    .filter_map(|a| Some((a.id, file.asset(a.id)?))),
            );
            *imp.doc.borrow_mut() = Some(CanvasDoc::new(note.id, &note.doc));
        }
        *imp.note.borrow_mut() = note;
        imp.fit_width.set(true);
        imp.settled_zoom.set(0.0);
        imp.current_page.set(0);
        self.fit_if_needed();
        self.configure_adjustments(Some((0.0, 0.0)));
        self.request_tiles();
        self.queue_draw();
        self.notify("page-count");
        self.notify("current-page");
    }

    pub fn current_page(&self) -> u32 {
        self.imp().current_page.get()
    }

    /// How often `page`'s ink was edited (or selected from) since the note was shown; page
    /// thumbnails compare it to know when they are stale.
    pub fn ink_generation(&self, page: PageId) -> u64 {
        self.imp().ink_gen.borrow().get(&page).copied().unwrap_or(0)
    }

    pub fn page_count(&self) -> u32 {
        self.imp()
            .doc
            .borrow()
            .as_ref()
            .map_or(0, |d| d.pages.len() as u32)
    }

    pub fn undo(&self) {
        tracing::debug!("undo");
        self.step_history(|note| {
            let crate::note::Note { history, doc, .. } = note;
            history.undo(doc)
        });
    }

    pub fn redo(&self) {
        tracing::debug!("redo");
        self.step_history(|note| {
            let crate::note::Note { history, doc, .. } = note;
            history.redo(doc)
        });
    }

    fn step_history(
        &self,
        step: impl FnOnce(&mut crate::note::Note) -> cera_core::Result<Option<Changes>>,
    ) {
        let Some(note) = self.imp().note.borrow().clone() else {
            return;
        };
        self.clear_selection();
        let result = step(&mut note.borrow_mut());
        match result {
            Ok(Some(changes)) => {
                self.apply_changes(&note, &changes);
                self.emit_by_name::<()>("changed", &[]);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("undo/redo failed: {e}"),
        }
    }

    /// Redraws what an undo or redo changed.
    fn apply_changes(&self, note: &SharedNote, changes: &Changes) {
        if changes.layout {
            self.layout_changed(note);
        }
        for &(page, area) in &changes.ink {
            self.invalidate_ink(page, area);
        }
        self.request_tiles();
        self.queue_draw();
    }

    /// Pages were added, removed, moved or changed paper.
    fn layout_changed(&self, note: &SharedNote) {
        {
            let note = note.borrow();
            *self.imp().doc.borrow_mut() = Some(CanvasDoc::new(note.id, &note.doc));
        }
        self.configure_adjustments(None);
        self.update_current_page();
        self.request_tiles();
        self.queue_draw();
        self.notify("page-count");
    }

    /// Runs a page edit (insert, delete, move, paper) through the note, then lays the pages
    /// out again. Returns the edit's result, or `None` if it failed.
    pub fn edit_pages<T>(
        &self,
        edit: impl FnOnce(&mut crate::note::Note) -> cera_core::Result<T>,
    ) -> Option<T> {
        self.clear_selection();
        let note = self.note()?;
        let result = edit(&mut note.borrow_mut());
        match result {
            Ok(value) => {
                self.layout_changed(&note);
                self.emit_by_name::<()>("changed", &[]);
                Some(value)
            }
            Err(e) => {
                tracing::warn!("page edit failed: {e}");
                None
            }
        }
    }

    /// Inserts a page after the current one, with paper from `spec_for(current page)`, and
    /// scrolls to it.
    pub fn add_page(&self, spec_for: impl FnOnce(&PageSpec) -> PageSpec) {
        let current = self.current_page() as usize;
        let index = self.edit_pages(|note| {
            let crate::note::Note { history, doc, .. } = note;
            let spec = spec_for(doc.page(current)?.spec());
            history.insert_page(doc, current + 1, spec)?;
            Ok(current + 1)
        });
        if let Some(index) = index {
            self.show_page(index);
        }
    }

    /// The bookmarked pages: index and name (empty if unnamed), in page order.
    pub fn bookmarks(&self) -> Vec<(usize, String)> {
        self.note().map_or_else(Vec::new, |note| {
            note.borrow()
                .doc
                .bookmarks()
                .map(|(i, p)| (i, p.bookmark().unwrap_or_default().to_owned()))
                .collect()
        })
    }

    pub fn is_bookmarked(&self) -> bool {
        let index = self.current_page() as usize;
        self.note().is_some_and(|note| {
            note.borrow()
                .doc
                .page(index)
                .is_ok_and(|p| p.bookmark().is_some())
        })
    }

    /// Bookmarks the current page, or removes its bookmark; saved like an edit, not undone.
    pub fn set_bookmarked(&self, bookmarked: bool) {
        let Some(note) = self.note() else { return };
        let index = self.current_page() as usize;
        let set = note
            .borrow_mut()
            .doc
            .set_bookmark(index, bookmarked.then(String::new));
        if let Err(e) = set {
            tracing::warn!("cannot bookmark page {index}: {e}");
            return;
        }
        self.emit_by_name::<()>("changed", &[]);
    }

    pub fn set_tile_budget(&self, bytes: usize) {
        self.imp().cache.borrow_mut().set_budget(bytes);
        self.queue_draw();
    }

    /// After an edit of `page` within `area` (points): its ink tiles there become stale and
    /// are asked for again. Returns the page's new ink generation.
    fn invalidate_ink(&self, page: PageId, area: Rect) -> u64 {
        let imp = self.imp();
        let generation = {
            let mut gens = imp.ink_gen.borrow_mut();
            let g = gens.entry(page).or_insert(0);
            *g += 1;
            *g
        };
        let doc = imp.doc.borrow();
        let Some(doc) = doc.as_ref() else {
            return generation;
        };
        let Some(index) = doc.page_index(page) else {
            return generation;
        };
        let spec = doc.pages[index].1;
        let target = self.target_scale();
        let area = area.expanded(2.0 / target.get() as f32);
        imp.cache
            .borrow_mut()
            .invalidate(&(doc.id, page, Layer::Ink), target, |key| {
                let grid = Grid::new((spec.width, spec.height), key.scale);
                tiles::tile_area(grid.tile_rect(key.tx, key.ty), key.scale).intersects(&area)
            });
        generation
    }

    /// Zooms by `steps` quarter-octaves (×1.19 each) around the centre of the view.
    pub fn zoom_step(&self, steps: i32) {
        let z = self.imp().zoom.get();
        let level = (z.log2() * 4.0).round() as i32 + steps;
        let centre = (
            f64::from(self.width()) / 2.0,
            f64::from(self.height()) / 2.0,
        );
        self.zoom_to(2f64.powf(f64::from(level) / 4.0), centre);
    }

    pub fn zoom_fit_width(&self) {
        let imp = self.imp();
        let Some(fit) = self.fit_zoom() else { return };
        let centre = (
            f64::from(self.width()) / 2.0,
            f64::from(self.height()) / 2.0,
        );
        self.zoom_to(fit, centre);
        imp.fit_width.set(true);
    }

    /// Short status for the `CERA_DEBUG` overlay; resets the frame-time window.
    pub fn debug_text(&self) -> String {
        let imp = self.imp();
        let cache = imp.cache.borrow();
        let mut stats = imp.stats.borrow_mut();
        let avg = if stats.frames > 0 {
            stats.total_us as f64 / f64::from(stats.frames) / 1000.0
        } else {
            0.0
        };
        let wet: usize = imp
            .wet
            .borrow()
            .iter()
            .map(WetStroke::tile_count)
            .sum::<usize>()
            + imp
                .active
                .borrow()
                .as_ref()
                .map_or(0, |a| a.wet.tile_count());
        let text = format!(
            "tiles pdf {} ink {} wet {} · {:.1}/{} MiB · queue {}\nframe {avg:.1} ms avg, {:.1} max · zoom {:.2} · RSS {}",
            cache.count(Layer::Pdf),
            cache.count(Layer::Ink),
            wet,
            cache.bytes() as f64 / f64::from(1 << 20),
            cache.budget() >> 20,
            self.renderer().queue_len(),
            stats.max_us as f64 / 1000.0,
            imp.zoom.get(),
            rss().map_or("?".into(), |kb| format!("{} MiB", kb / 1024)),
        );
        *stats = FrameStats {
            paint_start: stats.paint_start,
            ..FrameStats::default()
        };
        text
    }

    fn renderer(&self) -> &Renderer {
        self.imp()
            .renderer
            .get()
            .expect("set_renderer is called first")
    }

    /// A rendered tile for this canvas's note (the window routes results by note).
    pub fn deliver(&self, result: TileResult) {
        self.tile_ready(result);
    }

    fn setup_zoom_input(&self) {
        let pinch = gtk::GestureZoom::new();
        pinch.set_propagation_phase(gtk::PropagationPhase::Capture);
        pinch.connect_begin(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            move |g, _| {
                g.set_state(gtk::EventSequenceState::Claimed);
                let Some(centre) = g.bounding_box_center() else {
                    return;
                };
                let Some(view) = canvas.view() else { return };
                canvas.imp().pinch.set(Some(Pinch {
                    zoom: canvas.imp().zoom.get(),
                    anchor_pt: view.to_pt(centre.0, centre.1),
                }));
            }
        ));
        pinch.connect_scale_changed(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            move |g, scale| {
                let (Some(p), Some(centre)) = (canvas.imp().pinch.get(), g.bounding_box_center())
                else {
                    return;
                };
                canvas.apply_zoom(p.zoom * scale, p.anchor_pt, centre);
            }
        ));
        pinch.connect_end(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            move |_, _| canvas.imp().pinch.set(None)
        ));
        self.add_controller(pinch);

        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            move |_, x, y| canvas.imp().pointer.set((x, y))
        ));
        self.add_controller(motion);

        let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        wheel.connect_scroll(glib::clone!(
            #[weak(rename_to = canvas)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |c, _, dy| {
                if !c
                    .current_event_state()
                    .contains(gdk::ModifierType::CONTROL_MASK)
                {
                    return glib::Propagation::Proceed;
                }
                let z = canvas.imp().zoom.get();
                canvas.zoom_to(z * 2f64.powf(-dy / 4.0), canvas.imp().pointer.get());
                glib::Propagation::Stop
            }
        ));
        self.add_controller(wheel);
    }

    /// Two-finger zoom and pan driven from outside (when one finger draws): call with the
    /// fingers' centre, then with the scale since the start.
    pub fn pinch_begin(&self, centre: (f64, f64)) {
        let Some(view) = self.view() else { return };
        self.imp().pinch.set(Some(Pinch {
            zoom: self.imp().zoom.get(),
            anchor_pt: view.to_pt(centre.0, centre.1),
        }));
    }

    pub fn pinch_update(&self, scale: f64, centre: (f64, f64)) {
        if let Some(p) = self.imp().pinch.get() {
            self.apply_zoom(p.zoom * scale, p.anchor_pt, centre);
        }
    }

    pub fn pinch_end(&self) {
        self.imp().pinch.set(None);
    }

    fn set_adjustment(&self, orientation: gtk::Orientation, adj: Option<gtk::Adjustment>) {
        let imp = self.imp();
        let slot = match orientation {
            gtk::Orientation::Horizontal => &imp.hadjustment,
            _ => &imp.vadjustment,
        };
        if let Some((old, handler)) = slot.take() {
            old.disconnect(handler);
        }
        if let Some(adj) = adj {
            let handler = adj.connect_value_changed(glib::clone!(
                #[weak(rename_to = canvas)]
                self,
                move |_| canvas.scrolled()
            ));
            *slot.borrow_mut() = Some((adj, handler));
            self.configure_adjustments(None);
        }
    }

    fn adjustment(&self, orientation: gtk::Orientation) -> Option<gtk::Adjustment> {
        let imp = self.imp();
        let slot = match orientation {
            gtk::Orientation::Horizontal => &imp.hadjustment,
            _ => &imp.vadjustment,
        };
        slot.borrow().as_ref().map(|(a, _)| a.clone())
    }

    fn scroll(&self) -> (f64, f64) {
        let value = |o| self.adjustment(o).map_or(0.0, |a| a.value());
        (
            value(gtk::Orientation::Horizontal),
            value(gtk::Orientation::Vertical),
        )
    }

    /// Device pixels per logical pixel (fractional scaling aware).
    fn device_scale(&self) -> f64 {
        self.native()
            .and_then(|n| n.surface())
            .map_or(f64::from(self.scale_factor()), |s| s.scale())
    }

    /// Device pixels per point that tiles are rendered at.
    fn target_scale(&self) -> Scale {
        Scale::new(self.imp().settled_zoom.get() * self.device_scale())
    }

    fn view(&self) -> Option<View> {
        let doc = self.imp().doc.borrow();
        let doc = doc.as_ref()?;
        let scale = self.device_scale();
        // Scroll by whole device pixels so tiles land on the pixel grid.
        let (sx, sy) = self.scroll();
        let snap = |v: f64| (v * scale).round() / scale;
        Some(View::new(
            &doc.layout,
            self.imp().zoom.get(),
            (snap(sx), snap(sy)),
            (f64::from(self.width()), f64::from(self.height())),
        ))
    }

    fn fit_zoom(&self) -> Option<f64> {
        let doc = self.imp().doc.borrow();
        let doc = doc.as_ref()?;
        let width = f64::from(self.width());
        (width > 0.0 && doc.layout.len() > 0)
            .then(|| layout::fit_width_zoom(&doc.layout, width).clamp(MIN_ZOOM, MAX_ZOOM))
    }

    /// Keeps the fit-to-width zoom after a resize; the first fit renders right away.
    fn fit_if_needed(&self) {
        let imp = self.imp();
        if !imp.fit_width.get() {
            return;
        }
        let Some(fit) = self.fit_zoom() else { return };
        if (fit - imp.zoom.get()).abs() < 1e-9 && imp.settled_zoom.get() > 0.0 {
            return;
        }
        imp.zoom.set(fit);
        if imp.settled_zoom.get() == 0.0 {
            imp.settled_zoom.set(fit);
        } else {
            self.schedule_settle();
        }
    }

    /// Sets the adjustments' ranges from the content size; `scroll` moves to a new position.
    fn configure_adjustments(&self, scroll: Option<(f64, f64)>) {
        let imp = self.imp();
        let zoom = imp.zoom.get();
        let content = imp.doc.borrow().as_ref().map_or((0.0, 0.0), |d| {
            (d.layout.width() * zoom, d.layout.height() * zoom)
        });
        let size = (f64::from(self.width()), f64::from(self.height()));
        let axes = [
            (
                gtk::Orientation::Horizontal,
                content.0,
                size.0,
                scroll.map(|s| s.0),
            ),
            (
                gtk::Orientation::Vertical,
                content.1,
                size.1,
                scroll.map(|s| s.1),
            ),
        ];
        for (orientation, content, page, value) in axes {
            let Some(adj) = self.adjustment(orientation) else {
                continue;
            };
            let upper = content.max(page);
            let value = value.unwrap_or(adj.value()).clamp(0.0, upper - page);
            adj.configure(value, 0.0, upper, page * 0.1, page * 0.9, page);
        }
    }

    fn scrolled(&self) {
        self.queue_draw();
        self.request_tiles();
        self.update_current_page();
    }

    fn update_current_page(&self) {
        let page = {
            let doc = self.imp().doc.borrow();
            let (Some(doc), Some(view)) = (doc.as_ref(), self.view()) else {
                return;
            };
            let (_, y) = view.to_pt(0.0, view.height / 2.0);
            doc.layout.page_at(y) as u32
        };
        if self.imp().current_page.replace(page) != page {
            self.notify("current-page");
        }
    }

    fn zoom_to(&self, zoom: f64, anchor_view: (f64, f64)) {
        let Some(view) = self.view() else { return };
        self.apply_zoom(zoom, view.to_pt(anchor_view.0, anchor_view.1), anchor_view);
    }

    /// Zooms so that `anchor_pt` stays under `anchor_view`; tiles follow once it settles.
    fn apply_zoom(&self, zoom: f64, anchor_pt: (f64, f64), anchor_view: (f64, f64)) {
        let imp = self.imp();
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        imp.fit_width.set(false);
        imp.zoom.set(zoom);
        self.schedule_settle();
        let scroll = {
            let doc = imp.doc.borrow();
            let Some(doc) = doc.as_ref() else { return };
            layout::scroll_for_anchor(
                &doc.layout,
                zoom,
                f64::from(self.width()),
                anchor_pt,
                anchor_view,
            )
        };
        self.configure_adjustments(Some(scroll));
        self.queue_draw();
    }

    fn schedule_settle(&self) {
        let imp = self.imp();
        if let Some(id) = imp.settle.take() {
            id.remove();
        }
        let weak = self.downgrade();
        let id = glib::timeout_add_local_once(SETTLE, move || {
            let Some(canvas) = weak.upgrade() else { return };
            let imp = canvas.imp();
            imp.settle.take();
            imp.settled_zoom.set(imp.zoom.get());
            canvas.refresh_selection_texture();
            canvas.request_tiles();
            canvas.queue_draw();
        });
        *imp.settle.borrow_mut() = Some(id);
    }

    fn track_frame_times(&self) {
        let Some(clock) = self.frame_clock() else {
            return;
        };
        let weak = self.downgrade();
        clock.connect_before_paint(move |_| {
            if let Some(c) = weak.upgrade() {
                c.imp().stats.borrow_mut().paint_start = glib::monotonic_time();
            }
        });
        let weak = self.downgrade();
        clock.connect_after_paint(move |_| {
            if let Some(c) = weak.upgrade() {
                let mut s = c.imp().stats.borrow_mut();
                let us = glib::monotonic_time() - s.paint_start;
                s.frames += 1;
                s.total_us += us;
                s.max_us = s.max_us.max(us);
            }
        });
    }
}

/// Resident set size in KiB, from /proc.
fn rss() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}
