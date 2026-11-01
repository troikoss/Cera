//! Asking the render workers for tiles, and drawing a frame from the cache.
//!
//! Per page, back to front: paper (colour nodes), PDF tiles, ink tiles, wet ink, and the
//! eraser outline. PDF tiles are opaque, so while some are missing older zoom levels can show
//! underneath. Ink tiles are transparent, so a page's ink is drawn at one scale only: the
//! settled one once all its visible tiles are there, otherwise the closest one on hand.

use cera_core::{Background, PageId, PageSpec, PaperKind, Rect, Rgba8};
use cera_render::ink;
use cera_render::tile::{DocId, Grid, Layer, PixelRect, Scale, TileKey};
use cera_render::worker::{JobSource, TileJob, TileResult};
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, graphene, gsk};

use super::cache::TileCache;
use super::{Canvas, PIXEL_FORMAT};

/// Prefetch around the viewport, in device pixels.
const PREFETCH_X: f64 = 256.0;
const PREFETCH_Y: f64 = 512.0;
/// Eraser outline radius, logical pixels.
pub(super) const ERASER_RADIUS: f64 = 10.0;

/// The page area (points) covered by tile pixels `r` at `scale`.
pub(super) fn tile_area(r: PixelRect, scale: Scale) -> Rect {
    let px = 1.0 / scale.get();
    Rect {
        x0: (f64::from(r.x) * px) as f32,
        y0: (f64::from(r.y) * px) as f32,
        x1: (f64::from(r.x + r.width) * px) as f32,
        y1: (f64::from(r.y + r.height) * px) as f32,
    }
}

impl Canvas {
    /// Queues the missing and stale tiles at the settled zoom: visible ones first, then
    /// prefetch. Decodes the ink of pages coming into view.
    pub(super) fn request_tiles(&self) {
        let imp = self.imp();
        let Some(renderer) = imp.renderer.get() else {
            return;
        };
        if imp.settle.borrow().is_some() || !imp.visible.get() {
            return; // zoom still changing, or a hidden tab
        }
        let note = imp.note.borrow().clone();
        let doc = imp.doc.borrow();
        let (Some(doc), Some(note), Some(view)) = (doc.as_ref(), note, self.view()) else {
            return;
        };
        let mut note = note.borrow_mut();
        let scale = self.device_scale();
        let s = self.target_scale();
        let (margin_x, margin_y) = (PREFETCH_X / scale, PREFETCH_Y / scale);
        let mut cache = imp.cache.borrow_mut();
        let mut failed = imp.failed.borrow_mut();
        let gens = imp.ink_gen.borrow();
        let mut expected = imp.ink_expected.borrow_mut();
        expected.clear();

        let (_, y0, _, y1) = view.visible_pt();
        let pages = doc
            .layout
            .pages_between(y0 - margin_y / view.zoom, y1 + margin_y / view.zoom);
        let (mut visible, mut prefetch) = (Vec::new(), Vec::new());
        for i in pages {
            let (id, spec) = doc.pages[i];
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
            let inner = (ix0, iy0, ix1, iy1);
            let key = |layer, (tx, ty)| TileKey {
                doc: doc.id,
                page: id,
                layer,
                scale: s,
                tx,
                ty,
            };
            let mut push = |job: TileJob, rect: PixelRect| {
                if overlaps(rect, inner) {
                    visible.push(job);
                } else {
                    prefetch.push(job);
                }
            };

            if let Background::Pdf { asset, page } = spec.background
                && !failed.contains(&(doc.id, id, Layer::Pdf))
            {
                for t in grid.tiles_in(ox0, oy0, ox1, oy1) {
                    let key = key(Layer::Pdf, t);
                    if cache.is_fresh(&key) {
                        continue;
                    }
                    let job = TileJob {
                        key,
                        page_size: (spec.width, spec.height),
                        generation: 0,
                        source: JobSource::Pdf {
                            asset,
                            page,
                            fill: spec.paper.fill,
                        },
                    };
                    push(job, grid.tile_rect(t.0, t.1));
                }
            }

            if failed.contains(&(doc.id, id, Layer::Ink)) {
                continue;
            }
            let strokes = match note.doc.load_ink(i) {
                Ok(strokes) => strokes,
                Err(e) => {
                    tracing::warn!(page = id, "cannot read ink: {e}");
                    failed.insert((doc.id, id, Layer::Ink));
                    continue;
                }
            };
            if strokes.is_empty() {
                cache.remove_page(&(doc.id, id, Layer::Ink));
                continue;
            }
            let generation = gens.get(&id).copied().unwrap_or(0);
            let region = tile_area(
                PixelRect {
                    x: ox0.max(0.0) as u32,
                    y: oy0.max(0.0) as u32,
                    width: (ox1 - ox0.max(0.0)).max(0.0) as u32,
                    height: (oy1 - oy0.max(0.0)).max(0.0) as u32,
                },
                s,
            );
            let nearby = ink::strokes_in(strokes, region.expanded(2.0));
            let mut wanted = Vec::new();
            for t in grid.tiles_in(ox0, oy0, ox1, oy1) {
                let key = key(Layer::Ink, t);
                let rect = grid.tile_rect(t.0, t.1);
                if cache.is_fresh(&key) {
                    if overlaps(rect, inner) {
                        wanted.push(t);
                    }
                    continue;
                }
                let area = tile_area(rect, s).expanded((1.0 / s.get()) as f32);
                let in_tile = ink::strokes_in(&nearby, area);
                if in_tile.is_empty() {
                    cache.remove_tile(&key); // everything there was erased
                    continue;
                }
                if overlaps(rect, inner) {
                    wanted.push(t);
                }
                let job = TileJob {
                    key,
                    page_size: (spec.width, spec.height),
                    generation,
                    source: JobSource::Ink(in_tile),
                };
                push(job, rect);
            }
            expected.insert(id, (s, wanted));
        }
        tracing::trace!(
            visible = visible.len(),
            prefetch = prefetch.len(),
            "requesting tiles"
        );
        visible.append(&mut prefetch);
        renderer.set_jobs(doc.id, visible);
    }

    pub(super) fn tile_ready(&self, result: TileResult) {
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
                // Ink drawn before a later edit of the page goes in stale and is asked for again.
                let current = imp
                    .ink_gen
                    .borrow()
                    .get(&result.key.page)
                    .copied()
                    .unwrap_or(0);
                let stale = result.key.layer == Layer::Ink && result.generation < current;
                imp.cache.borrow_mut().insert(
                    result.key,
                    texture.upcast(),
                    p.bytes.len(),
                    result.generation,
                    stale,
                );
                if stale {
                    self.request_tiles();
                }
                self.queue_draw();
            }
            Err(e) => {
                let key = (result.key.doc, result.key.page, result.key.layer);
                if imp.failed.borrow_mut().insert(key) {
                    tracing::warn!(page = result.key.page, layer = ?result.key.layer, "cannot render: {e}");
                }
            }
        }
    }

    pub(super) fn draw(&self, snapshot: &gtk::Snapshot) {
        let imp = self.imp();
        let doc = imp.doc.borrow();
        let (Some(doc), Some(view)) = (doc.as_ref(), self.view()) else {
            return;
        };
        let scale = self.device_scale();
        let snap = |v: f64| ((v * scale).round() / scale) as f32;
        let target = self.target_scale();
        let mut cache = imp.cache.borrow_mut();
        cache.begin_frame();
        let expected = imp.ink_expected.borrow();
        let mut wet = imp.wet.borrow_mut();
        let active = imp.active.borrow();
        // Wet strokes from an old zoom level would never be matched by a tile: drop them.
        wet.retain(|w| w.scale == target);

        let (_, y0, _, y1) = view.visible_pt();
        let mut covered = Vec::new();
        let mut on_screen = Vec::new();
        for i in doc.layout.pages_between(y0, y1) {
            let (id, spec) = doc.pages[i];
            on_screen.push(id);
            let r = doc.layout.page_rect(i);
            let (vx, vy) = view.to_view(r.x, r.y);
            let page = graphene::Rect::new(
                snap(vx),
                snap(vy),
                (r.w * view.zoom) as f32,
                (r.h * view.zoom) as f32,
            );
            draw_paper(snapshot, &spec, &page, view.zoom, scale);
            let tiles = |layer| PageTiles {
                doc: doc.id,
                page: id,
                layer,
                size: (spec.width, spec.height),
                rect: page,
                zoom: view.zoom,
                viewport: (view.width, view.height),
            };
            if let Background::Pdf { .. } = spec.background
                && tiles(Layer::Pdf).draw_opaque(snapshot, &mut cache, target)
            {
                covered.push((doc.id, id, Layer::Pdf));
            }
            if tiles(Layer::Ink).draw_transparent(snapshot, &mut cache, target, expected.get(&id)) {
                covered.push((doc.id, id, Layer::Ink));
            }
            for w in wet.iter().chain(active.as_ref().map(|a| &a.wet)) {
                if w.page == id {
                    draw_wet(snapshot, &cache, doc.id, w, &page, view.zoom);
                }
            }
        }
        // Older zoom levels were only needed until the settled one arrived.
        for page in covered {
            cache.remove_other_scales(&page, target);
        }
        // Finished strokes leave the wet layer once every tile they touch has caught up, or
        // once their page is off screen (its tiles are re-rendered when it comes back).
        wet.retain(|w| on_screen.contains(&w.page) && !caught_up(&cache, doc.id, w));

        if let Some((x, y)) = imp.eraser_cursor.get() {
            let r = ERASER_RADIUS as f32;
            let bounds = graphene::Rect::new(x as f32 - r, y as f32 - r, 2.0 * r, 2.0 * r);
            let ring = gsk::RoundedRect::from_rect(bounds, r);
            let grey = gdk::RGBA::new(0.3, 0.3, 0.3, 0.8);
            snapshot.append_border(&ring, &[1.5; 4], &[grey; 4]);
        }
    }
}

/// The ink tile under a wet tile, if it is up to date for the wet stroke.
fn ink_key(doc: DocId, w: &super::WetStroke, (tx, ty): (u16, u16)) -> TileKey {
    TileKey {
        doc,
        page: w.page,
        layer: Layer::Ink,
        scale: w.scale,
        tx,
        ty,
    }
}

fn tile_has_caught_up(
    cache: &TileCache<gdk::Texture>,
    doc: DocId,
    w: &super::WetStroke,
    t: (u16, u16),
) -> bool {
    match w.generation {
        Some(g) => cache
            .fresh_generation(&ink_key(doc, w, t))
            .is_some_and(|fg| fg >= g),
        None => false, // still being drawn
    }
}

fn caught_up(cache: &TileCache<gdk::Texture>, doc: DocId, w: &super::WetStroke) -> bool {
    w.generation.is_some()
        && w.tiles()
            .all(|(t, ..)| tile_has_caught_up(cache, doc, w, t))
}

/// Wet tiles not yet replaced by finished ones; translucent strokes as one group.
fn draw_wet(
    snapshot: &gtk::Snapshot,
    cache: &TileCache<gdk::Texture>,
    doc: DocId,
    w: &super::WetStroke,
    page: &graphene::Rect,
    zoom: f64,
) {
    let opacity = w.opacity();
    if opacity < 1.0 {
        snapshot.push_opacity(f64::from(opacity));
    }
    let k = zoom / w.scale.get();
    for (t, r, texture) in w.tiles() {
        if tile_has_caught_up(cache, doc, w, t) {
            continue;
        }
        let rect = graphene::Rect::new(
            page.x() + (f64::from(r.x) * k) as f32,
            page.y() + (f64::from(r.y) * k) as f32,
            (f64::from(r.width) * k) as f32,
            (f64::from(r.height) * k) as f32,
        );
        snapshot.append_texture(texture, &rect);
    }
    if opacity < 1.0 {
        snapshot.pop();
    }
}

/// One page's tiles of one layer as drawn this frame.
struct PageTiles {
    doc: DocId,
    page: PageId,
    layer: Layer,
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
                layer: self.layer,
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

    fn draw_scale(
        &self,
        snapshot: &gtk::Snapshot,
        cache: &mut TileCache<gdk::Texture>,
        scale: Scale,
    ) {
        for (key, rect) in self.visible(scale) {
            if let Some(texture) = cache.get(&key) {
                snapshot.append_texture(texture, &rect);
            }
        }
    }

    /// Draws tiles at `target`; where some are missing, tiles at other scales go underneath.
    /// Returns whether `target` covered the visible part on its own.
    fn draw_opaque(
        &self,
        snapshot: &gtk::Snapshot,
        cache: &mut TileCache<gdk::Texture>,
        target: Scale,
    ) -> bool {
        let complete = self.visible(target).all(|(key, _)| cache.contains(&key));
        if !complete {
            let mut others: Vec<Scale> = cache
                .scales(&(self.doc, self.page, self.layer))
                .filter(|&s| s != target)
                .collect();
            // Closest to the target last, so it ends up on top.
            others.sort_by(|a, b| distance(*b, target).total_cmp(&distance(*a, target)));
            for scale in others {
                self.draw_scale(snapshot, cache, scale);
            }
        }
        self.draw_scale(snapshot, cache, target);
        complete
    }

    /// Draws ink at one scale: `target` once every visible tile that should exist there does
    /// (`expected`), otherwise the closest scale on hand. Returns whether `target` was complete.
    fn draw_transparent(
        &self,
        snapshot: &gtk::Snapshot,
        cache: &mut TileCache<gdk::Texture>,
        target: Scale,
        expected: Option<&super::ExpectedInk>,
    ) -> bool {
        let complete = expected.is_some_and(|(s, tiles)| {
            *s == target
                && tiles.iter().all(|&(tx, ty)| {
                    cache.contains(&TileKey {
                        doc: self.doc,
                        page: self.page,
                        layer: self.layer,
                        scale: target,
                        tx,
                        ty,
                    })
                })
        });
        let scale = if complete {
            Some(target)
        } else {
            cache
                .scales(&(self.doc, self.page, self.layer))
                .min_by(|a, b| distance(*a, target).total_cmp(&distance(*b, target)))
        };
        if let Some(scale) = scale {
            self.draw_scale(snapshot, cache, scale);
        }
        complete
    }
}

fn distance(s: Scale, target: Scale) -> f64 {
    (s.get() / target.get()).ln().abs()
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
            let rule = graphene::Rect::new(x, y + spacing - width, page.width(), width);
            snapshot.append_color(&line, &rule);
        }
        PaperKind::Grid => {
            let h = graphene::Rect::new(x, y + spacing - width, spacing, width);
            let v = graphene::Rect::new(x + spacing - width, y, width, spacing);
            snapshot.append_color(&line, &h);
            snapshot.append_color(&line, &v);
        }
        PaperKind::Dots => {
            let dot = ((1.5 * zoom) as f32).max(2.0 * px);
            let r = graphene::Rect::new(x + spacing - dot, y + spacing - dot, dot, dot);
            snapshot.append_color(&line, &r);
        }
        PaperKind::Blank => {}
    }
    snapshot.pop();
}
