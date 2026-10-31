//! The note canvas: pages stacked vertically inside a `GtkScrolledWindow`, drawn from cached
//! tile textures (DESIGN.md §5). Implements `GtkScrollable`.
//!
//! - Paper is a fill colour plus a repeat node for lines, grid or dots: no textures.
//! - PDF backgrounds come from the render workers as tiles at the *settled* zoom. While the
//!   zoom changes, the tiles on hand are scaled by the GPU; new ones are requested once it has
//!   been still for 150 ms.
//! - `snapshot` only appends cached textures and colour nodes.

mod cache;
mod layout;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashSet;
use std::time::Duration;

use cera_core::{Background, Document, PageId, PageSpec, PaperKind, Rgba8};
use cera_render::tile::{DocId, Grid, Layer, PixelRect, Scale, TileKey};
use cera_render::worker::{Renderer, TileJob, TileResult};
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib, graphene, gsk};

use self::cache::TileCache;
use self::layout::{Layout, View};

/// Tile cache budget (DESIGN.md §5). It counts the CPU copy of each texture; GSK keeps GPU
/// copies of recently drawn tiles on top.
const TILE_BUDGET: usize = 128 << 20;
/// Request new tiles this long after the zoom stops changing.
const SETTLE: Duration = Duration::from_millis(150);
/// Prefetch around the viewport, in device pixels.
const PREFETCH_X: f64 = 256.0;
const PREFETCH_Y: f64 = 512.0;
/// Logical pixels per point.
const MIN_ZOOM: f64 = 0.1;
const MAX_ZOOM: f64 = 8.0;

#[cfg(target_endian = "little")]
const PIXEL_FORMAT: gdk::MemoryFormat = gdk::MemoryFormat::B8g8r8a8Premultiplied;
#[cfg(target_endian = "big")]
const PIXEL_FORMAT: gdk::MemoryFormat = gdk::MemoryFormat::A8r8g8b8Premultiplied;

/// What the canvas shows: the pages of an open note.
pub struct CanvasDoc {
    pub id: DocId,
    pages: Vec<(PageId, PageSpec)>,
    layout: Layout,
}

impl CanvasDoc {
    pub fn new(id: DocId, doc: &Document) -> Self {
        let pages: Vec<_> = doc.pages().iter().map(|p| (p.id(), *p.spec())).collect();
        let layout = Layout::new(pages.iter().map(|(_, s)| (s.width, s.height)));
        Self { id, pages, layout }
    }
}

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
        pub(super) doc: RefCell<Option<CanvasDoc>>,
        pub(super) renderer: OnceCell<Renderer>,
        pub(super) cache: RefCell<TileCache<gdk::Texture>>,
        pub(super) zoom: Cell<f64>,
        /// Zoom the tiles are rendered for; lags `zoom` while it changes. 0 until laid out.
        pub(super) settled_zoom: Cell<f64>,
        pub(super) fit_width: Cell<bool>,
        pub(super) settle: RefCell<Option<glib::SourceId>>,
        pub(super) failed: RefCell<HashSet<(DocId, PageId)>>,
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
                doc: RefCell::default(),
                renderer: OnceCell::new(),
                cache: RefCell::new(TileCache::new(TILE_BUDGET)),
                zoom: Cell::new(1.0),
                settled_zoom: Cell::new(0.0),
                fit_width: Cell::new(true),
                settle: RefCell::default(),
                failed: RefCell::default(),
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
            use std::sync::OnceLock;
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

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "hadjustment" => self
                    .obj()
                    .set_adjustment(gtk::Orientation::Horizontal, value.get().unwrap()),
                "vadjustment" => self
                    .obj()
                    .set_adjustment(gtk::Orientation::Vertical, value.get().unwrap()),
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
            obj.setup_renderer();
            obj.setup_input();

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
            self.obj().fit_if_needed();
            self.obj().configure_adjustments(None);
            self.obj().request_tiles();
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
    /// Shows `doc` (or nothing), fitted to the width and scrolled to the top.
    pub fn set_document(&self, doc: Option<CanvasDoc>) {
        let imp = self.imp();
        if let Some(old) = imp.doc.take() {
            imp.cache.borrow_mut().remove_doc(old.id);
            imp.failed.borrow_mut().retain(|(d, _)| *d != old.id);
        }
        *imp.doc.borrow_mut() = doc;
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

    /// The canvas's render threads; give them a note's assets with `open_doc` before showing it.
    pub fn renderer(&self) -> &Renderer {
        self.imp().renderer.get().expect("set up in constructed")
    }

    pub fn current_page(&self) -> u32 {
        self.imp().current_page.get()
    }

    pub fn page_count(&self) -> u32 {
        self.imp()
            .doc
            .borrow()
            .as_ref()
            .map_or(0, |d| d.pages.len() as u32)
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
        imp.fit_width.set(true);
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
        let text = format!(
            "tiles {} · {:.1}/{} MiB · queue {}\nframe {avg:.1} ms avg, {:.1} max · zoom {:.2} · RSS {}",
            cache.len(),
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

    fn setup_renderer(&self) {
        let threads =
            std::thread::available_parallelism().map_or(1, |n| if n.get() >= 4 { 2 } else { 1 });
        let (tx, rx) = async_channel::unbounded::<TileResult>();
        let _ = self.imp().renderer.set(Renderer::new(threads, tx));
        // Holds only a weak reference, so the canvas (and with it the renderer and its
        // senders) can go away; the channel then closes and this loop ends.
        let weak = self.downgrade();
        glib::spawn_future_local(async move {
            while let Ok(result) = rx.recv().await {
                let Some(canvas) = weak.upgrade() else { break };
                canvas.tile_ready(result);
            }
        });
    }

    fn setup_input(&self) {
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
            canvas.request_tiles();
            canvas.queue_draw();
        });
        *imp.settle.borrow_mut() = Some(id);
    }

    /// Queues the missing tiles at the settled zoom: visible ones first, then prefetch.
    fn request_tiles(&self) {
        let imp = self.imp();
        let Some(renderer) = imp.renderer.get() else {
            return;
        };
        if imp.settle.borrow().is_some() {
            return; // zoom still changing
        }
        let doc = imp.doc.borrow();
        let (Some(doc), Some(view)) = (doc.as_ref(), self.view()) else {
            renderer.set_jobs(Vec::new());
            return;
        };
        let scale = self.device_scale();
        let s = Scale::new(imp.settled_zoom.get() * scale);
        let (margin_x, margin_y) = (PREFETCH_X / scale, PREFETCH_Y / scale);
        let cache = imp.cache.borrow();
        let failed = imp.failed.borrow();

        let (_, y0, _, y1) = view.visible_pt();
        let pages = doc
            .layout
            .pages_between(y0 - margin_y / view.zoom, y1 + margin_y / view.zoom);
        let (mut visible, mut prefetch) = (Vec::new(), Vec::new());
        for i in pages {
            let (id, spec) = doc.pages[i];
            let Background::Pdf { asset, page } = spec.background else {
                continue;
            };
            if failed.contains(&(doc.id, id)) {
                continue;
            }
            let grid = Grid::new((spec.width, spec.height), s);
            let r = doc.layout.page_rect(i);
            let (px, py) = view.to_view(r.x, r.y);
            // View coordinates → this page's pixel grid.
            let to_grid = |vx: f64, vy: f64| {
                (
                    (vx - px) / view.zoom * s.get(),
                    (vy - py) / view.zoom * s.get(),
                )
            };
            let (ix0, iy0) = to_grid(0.0, 0.0);
            let (ix1, iy1) = to_grid(view.width, view.height);
            let (ox0, oy0) = to_grid(-margin_x, -margin_y);
            let (ox1, oy1) = to_grid(view.width + margin_x, view.height + margin_y);
            for (tx, ty) in grid.tiles_in(ox0, oy0, ox1, oy1) {
                let key = TileKey {
                    doc: doc.id,
                    page: id,
                    layer: Layer::Pdf,
                    scale: s,
                    tx,
                    ty,
                };
                if cache.contains(&key) {
                    continue;
                }
                let job = TileJob {
                    key,
                    page_size: (spec.width, spec.height),
                    asset,
                    pdf_page: page,
                    fill: spec.paper.fill,
                };
                if overlaps(grid.tile_rect(tx, ty), (ix0, iy0, ix1, iy1)) {
                    visible.push(job);
                } else {
                    prefetch.push(job);
                }
            }
        }
        tracing::debug!(
            visible = visible.len(),
            prefetch = prefetch.len(),
            scale = s.get(),
            "requesting tiles"
        );
        visible.append(&mut prefetch);
        renderer.set_jobs(visible);
    }

    fn tile_ready(&self, result: TileResult) {
        let imp = self.imp();
        if imp.doc.borrow().as_ref().map(|d| d.id) != Some(result.key.doc) {
            return;
        }
        match result.pixels {
            Ok(p) => {
                let texture = gdk::MemoryTexture::new(
                    p.width as i32,
                    p.height as i32,
                    PIXEL_FORMAT,
                    &p.bytes,
                    p.stride,
                );
                imp.cache
                    .borrow_mut()
                    .insert(result.key, texture.upcast(), p.bytes.len());
                self.queue_draw();
            }
            Err(e) => {
                if imp
                    .failed
                    .borrow_mut()
                    .insert((result.key.doc, result.key.page))
                {
                    tracing::warn!(page = result.key.page, "cannot render PDF page: {e}");
                }
            }
        }
    }

    fn draw(&self, snapshot: &gtk::Snapshot) {
        let imp = self.imp();
        let doc = imp.doc.borrow();
        let (Some(doc), Some(view)) = (doc.as_ref(), self.view()) else {
            return;
        };
        let scale = self.device_scale();
        let snap = |v: f64| ((v * scale).round() / scale) as f32;
        let target = Scale::new(imp.settled_zoom.get() * scale);
        let mut cache = imp.cache.borrow_mut();
        cache.begin_frame();

        let (_, y0, _, y1) = view.visible_pt();
        let mut covered = Vec::new();
        for i in doc.layout.pages_between(y0, y1) {
            let (id, spec) = doc.pages[i];
            let r = doc.layout.page_rect(i);
            let (vx, vy) = view.to_view(r.x, r.y);
            let page = graphene::Rect::new(
                snap(vx),
                snap(vy),
                (r.w * view.zoom) as f32,
                (r.h * view.zoom) as f32,
            );
            draw_paper(snapshot, &spec, &page, view.zoom, scale);
            if let Background::Pdf { .. } = spec.background {
                let tiles = PageTiles {
                    doc: doc.id,
                    page: id,
                    size: (spec.width, spec.height),
                    rect: page,
                    zoom: view.zoom,
                    viewport: (view.width, view.height),
                };
                if tiles.draw(snapshot, &mut cache, target) {
                    covered.push((doc.id, id, Layer::Pdf));
                }
            }
        }
        // Older zoom levels were only needed until the settled one arrived.
        for page in covered {
            cache.remove_other_scales(&page, target);
        }
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

/// One page's PDF tiles as drawn this frame.
struct PageTiles {
    doc: DocId,
    page: PageId,
    size: (f32, f32),
    /// Page rectangle in view coordinates.
    rect: graphene::Rect,
    zoom: f64,
    viewport: (f64, f64),
}

impl PageTiles {
    /// Tiles at `scale` that cover the visible part of the page, with their view rectangles.
    fn visible(&self, scale: Scale) -> impl Iterator<Item = (TileKey, graphene::Rect)> + '_ {
        let grid = Grid::new(self.size, scale);
        let s = scale.get();
        let (px, py) = (f64::from(self.rect.x()), f64::from(self.rect.y()));
        let to_grid = |v: f64, origin: f64| (v - origin) / self.zoom * s;
        let tiles = grid.tiles_in(
            to_grid(0.0, px),
            to_grid(0.0, py),
            to_grid(self.viewport.0, px),
            to_grid(self.viewport.1, py),
        );
        let k = self.zoom / s;
        tiles.map(move |(tx, ty)| {
            let r = grid.tile_rect(tx, ty);
            let key = TileKey {
                doc: self.doc,
                page: self.page,
                layer: Layer::Pdf,
                scale,
                tx,
                ty,
            };
            let rect = graphene::Rect::new(
                (px + f64::from(r.x) * k) as f32,
                (py + f64::from(r.y) * k) as f32,
                (f64::from(r.width) * k) as f32,
                (f64::from(r.height) * k) as f32,
            );
            (key, rect)
        })
    }

    /// Draws tiles at `target`; where some are missing, tiles at other scales go underneath.
    /// Returns whether `target` covered the visible part on its own.
    fn draw(
        &self,
        snapshot: &gtk::Snapshot,
        cache: &mut TileCache<gdk::Texture>,
        target: Scale,
    ) -> bool {
        let wanted: Vec<_> = self.visible(target).collect();
        let complete = wanted.iter().all(|(key, _)| cache.contains(key));
        if !complete {
            let mut others: Vec<Scale> = cache
                .scales(&(self.doc, self.page, Layer::Pdf))
                .filter(|&s| s != target)
                .collect();
            // Closest to the target last, so it ends up on top.
            let distance = |s: &Scale| (s.get() / target.get()).ln().abs();
            others.sort_by(|a, b| distance(b).total_cmp(&distance(a)));
            for scale in others {
                for (key, rect) in self.visible(scale) {
                    if let Some(texture) = cache.get(&key) {
                        snapshot.append_texture(texture, &rect);
                    }
                }
            }
        }
        for (key, rect) in &wanted {
            if let Some(texture) = cache.get(key) {
                snapshot.append_texture(texture, rect);
            }
        }
        complete
    }
}

fn overlaps(r: PixelRect, (x0, y0, x1, y1): (f64, f64, f64, f64)) -> bool {
    f64::from(r.x) < x1
        && f64::from(r.x + r.width) > x0
        && f64::from(r.y) < y1
        && f64::from(r.y + r.height) > y0
}

fn rgba(c: Rgba8) -> gdk::RGBA {
    gdk::RGBA::new(
        f32::from(c.r) / 255.0,
        f32::from(c.g) / 255.0,
        f32::from(c.b) / 255.0,
        f32::from(c.a) / 255.0,
    )
}

/// Page edge, fill and pattern: colour nodes only, the pattern as one repeat node.
fn draw_paper(
    snapshot: &gtk::Snapshot,
    spec: &PageSpec,
    page: &graphene::Rect,
    zoom: f64,
    scale: f64,
) {
    let px = (1.0 / scale) as f32;
    let edge = graphene::Rect::new(
        page.x() - px,
        page.y() - px,
        page.width() + 2.0 * px,
        page.height() + 2.0 * px,
    );
    let shade = gdk::RGBA::new(0.0, 0.0, 0.0, 0.18);
    snapshot.append_border(
        &gsk::RoundedRect::from_rect(edge, 0.0),
        &[px; 4],
        &[shade; 4],
    );
    snapshot.append_color(&rgba(spec.paper.fill), page);

    let paper = spec.paper;
    let spacing = (f64::from(paper.spacing) * zoom) as f32;
    if paper.kind == PaperKind::Blank || spacing < 2.0 {
        return;
    }
    let line = rgba(paper.line);
    let width = ((0.5 * zoom) as f32).max(px);
    let (x, y) = (page.x(), page.y());
    let cell = match paper.kind {
        PaperKind::Ruled => graphene::Rect::new(x, y, page.width(), spacing),
        _ => graphene::Rect::new(x, y, spacing, spacing),
    };
    snapshot.push_repeat(page, Some(&cell));
    match paper.kind {
        PaperKind::Ruled => {
            snapshot.append_color(
                &line,
                &graphene::Rect::new(x, y + spacing - width, page.width(), width),
            );
        }
        PaperKind::Grid => {
            snapshot.append_color(
                &line,
                &graphene::Rect::new(x, y + spacing - width, spacing, width),
            );
            snapshot.append_color(
                &line,
                &graphene::Rect::new(x + spacing - width, y, width, spacing),
            );
        }
        PaperKind::Dots => {
            let dot = ((1.5 * zoom) as f32).max(2.0 * px);
            snapshot.append_color(
                &line,
                &graphene::Rect::new(x + spacing - dot, y + spacing - dot, dot, dot),
            );
        }
        PaperKind::Blank => {}
    }
    snapshot.pop();
}

/// Resident set size in KiB, from /proc.
fn rss() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}
