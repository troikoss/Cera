// SPDX-License-Identifier: GPL-3.0-or-later
//! Lasso selection (DESIGN.md §9): draw a loop (or, in rectangle mode, drag out a rectangle)
//! on a page to select the strokes mostly inside it, then drag to move them, or delete,
//! recolour, reorder, cut, copy and paste (also into other notes).
//!
//! While strokes are selected they are left out of the page's ink tiles and drawn from one
//! texture, rendered once when the selection is made, so dragging costs nothing per frame
//! and no tile is redrawn until the selection ends. Deselected strokes go back into the
//! tiles; their texture stays as a "ghost" until those tiles have caught up, like wet ink.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cera_core::{Arrange, PageId, Rect, Rgba8, Stroke, Tool, arrange, polygon_contains};
use cera_render::ink;
use cera_render::tile::{DocId, Grid, Layer, Scale, TileKey};
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{cairo, gdk, glib, graphene, gsk};

use super::cache::TileCache;
use super::tiles::tile_area;
use super::{Canvas, PIXEL_FORMAT};

/// Longest side of a selection texture in device pixels (16 MiB at most); bigger selections
/// render coarser.
const MAX_TEXTURE_PX: f64 = 2048.0;
/// A ghost goes after this long even if not all of its tiles were seen again.
const GHOST_LIFETIME: Duration = Duration::from_secs(3);
/// Lasso points closer than this (logical px) to the previous one are skipped.
const MIN_STEP_PX: f64 = 2.0;
/// How far outside the selection a drag still grabs it, logical px.
const GRAB_MARGIN_PX: f64 = 16.0;

fn accent() -> gdk::RGBA {
    gdk::RGBA::new(0.21, 0.52, 0.89, 1.0)
}

/// The loop being drawn, in page points. A rectangle is kept as its four corners, starting
/// where the drag began.
pub(super) struct LassoPath {
    page: PageId,
    index: usize,
    points: Vec<(f32, f32)>,
    rect: bool,
}

/// Strokes drawn into one texture, and the page area (points) it covers.
pub(super) struct InkTexture {
    texture: gdk::Texture,
    rect: Rect,
}

pub(super) struct Selection {
    pub(super) page: PageId,
    /// Ascending indices into the page's strokes.
    indices: Vec<usize>,
    pub(super) strokes: Vec<Arc<Stroke>>,
    bounds: Rect,
    /// Drag offset in points, applied on release.
    offset: (f32, f32),
    /// Where the current drag started (page points).
    drag_from: Option<(f32, f32)>,
    texture: Option<InkTexture>,
}

/// Deselected strokes shown from their texture until the tiles under them include them.
pub(super) struct Ghost {
    page: PageId,
    page_size: (f32, f32),
    ink: InkTexture,
    areas: Vec<Rect>,
    scale: Scale,
    generation: u64,
    born: Instant,
}

thread_local! {
    /// Cut or copied strokes, shared by every note in the process.
    static CLIPBOARD: RefCell<Vec<Arc<Stroke>>> = const { RefCell::new(Vec::new()) };
}

pub fn has_clipboard() -> bool {
    CLIPBOARD.with(|c| !c.borrow().is_empty())
}

impl Canvas {
    pub fn has_selection(&self) -> bool {
        self.imp().selection.borrow().is_some()
    }

    /// A lasso loop or a drag of the selection is in progress.
    pub(super) fn lasso_active(&self) -> bool {
        let imp = self.imp();
        imp.lasso.borrow().is_some()
            || imp
                .selection
                .borrow()
                .as_ref()
                .is_some_and(|s| s.drag_from.is_some())
    }

    /// The page under view point (`x`, `y`) and the point in its coordinates.
    fn page_point(&self, x: f64, y: f64) -> Option<(usize, PageId, (f32, f32))> {
        let view = self.view()?;
        let (sx, sy) = view.to_pt(x, y);
        let doc = self.imp().doc.borrow();
        let doc = doc.as_ref()?;
        doc.layout.pages_between(sy, sy).find_map(|i| {
            let r = doc.layout.page_rect(i);
            (sx >= r.x && sx <= r.x + r.w)
                .then(|| (i, doc.pages[i].0, ((sx - r.x) as f32, (sy - r.y) as f32)))
        })
    }

    /// `(x, y)` in the coordinates of `page`, even outside it.
    fn point_on(&self, page: PageId, x: f64, y: f64) -> Option<(f32, f32)> {
        let view = self.view()?;
        let (sx, sy) = view.to_pt(x, y);
        let doc = self.imp().doc.borrow();
        let doc = doc.as_ref()?;
        let r = doc.layout.page_rect(doc.page_index(page)?);
        Some(((sx - r.x) as f32, (sy - r.y) as f32))
    }

    pub(super) fn lasso_begin(&self, x: f64, y: f64) {
        let imp = self.imp();
        let zoom = imp.zoom.get();
        let grab = {
            let mut sel = imp.selection.borrow_mut();
            sel.as_mut().and_then(|s| {
                let p = self.point_on(s.page, x, y)?;
                let margin = (GRAB_MARGIN_PX / zoom) as f32;
                let area = s.bounds.translated(s.offset.0, s.offset.1).expanded(margin);
                area.contains(p).then(|| s.drag_from = Some(p))
            })
        };
        if grab.is_some() {
            return;
        }
        self.clear_selection();
        if let Some((index, page, p)) = self.page_point(x, y) {
            *imp.lasso.borrow_mut() = Some(LassoPath {
                page,
                index,
                points: vec![p],
                rect: self.tools().rect_select.get(),
            });
            self.queue_draw();
        }
    }

    pub(super) fn lasso_extend(&self, x: f64, y: f64) {
        let imp = self.imp();
        let min_step = (MIN_STEP_PX / imp.zoom.get()) as f32;
        if let Some(sel) = imp.selection.borrow_mut().as_mut()
            && let Some(from) = sel.drag_from
        {
            if let Some(p) = self.point_on(sel.page, x, y) {
                sel.offset = (p.0 - from.0, p.1 - from.1);
            }
        } else if let Some(path) = imp.lasso.borrow_mut().as_mut()
            && let Some(p) = self.point_on(path.page, x, y)
        {
            if path.rect {
                let (x0, y0) = path.points[0];
                path.points = vec![(x0, y0), (p.0, y0), p, (x0, p.1)];
                self.queue_draw();
                return;
            }
            let last = *path.points.last().expect("starts with a point");
            if (p.0 - last.0).hypot(p.1 - last.1) >= min_step {
                path.points.push(p);
            }
        }
        self.queue_draw();
    }

    pub(super) fn lasso_end(&self) {
        let imp = self.imp();
        let dragged = imp.selection.borrow_mut().as_mut().and_then(|s| {
            s.drag_from.take()?;
            Some(std::mem::take(&mut s.offset))
        });
        if let Some(offset) = dragged {
            if offset != (0.0, 0.0) {
                self.move_selection(offset);
            }
            self.queue_draw();
            return;
        }
        if let Some(path) = imp.lasso.take() {
            self.select_inside(&path);
            self.queue_draw();
        }
    }

    /// Drops a half-drawn loop or drag (a touch that turned into a pinch).
    pub(super) fn lasso_cancel(&self) {
        let imp = self.imp();
        imp.lasso.take();
        if let Some(s) = imp.selection.borrow_mut().as_mut() {
            s.drag_from = None;
            s.offset = (0.0, 0.0);
        }
        self.queue_draw();
    }

    /// Selects the strokes with more than half of their points inside the loop.
    fn select_inside(&self, path: &LassoPath) {
        if path.points.len() < 3 {
            return;
        }
        let Some(note) = self.note() else { return };
        let indices: Vec<usize> = {
            let mut note = note.borrow_mut();
            let Ok(strokes) = note.doc.load_ink(path.index) else {
                return;
            };
            strokes
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    let inside = s
                        .points()
                        .iter()
                        .filter(|p| polygon_contains(&path.points, (p.x, p.y)))
                        .count();
                    inside * 2 > s.points().len()
                })
                .map(|(i, _)| i)
                .collect()
        };
        tracing::debug!(
            points = path.points.len(),
            selected = indices.len(),
            "lasso"
        );
        if !indices.is_empty() {
            self.select(path.page, indices);
        }
    }

    /// Makes strokes `indices` of `page` the selection: they leave the ink tiles and show
    /// from a texture.
    fn select(&self, page: PageId, indices: Vec<usize>) {
        let Some(note) = self.note() else { return };
        let strokes: Vec<Arc<Stroke>> = {
            let mut note = note.borrow_mut();
            let Some(index) = note.doc.page_index(page) else {
                return;
            };
            let Ok(all) = note.doc.load_ink(index) else {
                return;
            };
            indices
                .iter()
                .filter_map(|&i| all.get(i).cloned())
                .collect()
        };
        let Some(bounds) = strokes.iter().map(|s| s.bounds()).reduce(Rect::union) else {
            return;
        };
        let texture = render_strokes(&strokes, bounds, self.target_scale().get());
        *self.imp().selection.borrow_mut() = Some(Selection {
            page,
            indices,
            strokes,
            bounds,
            offset: (0.0, 0.0),
            drag_from: None,
            texture,
        });
        self.invalidate_ink(page, bounds);
        self.request_tiles();
        self.queue_draw();
        self.emit_by_name::<()>("selection-changed", &[]);
    }

    /// Ends the selection; its strokes go back into the tiles.
    pub fn clear_selection(&self) {
        let imp = self.imp();
        imp.lasso.take();
        let Some(sel) = imp.selection.take() else {
            return;
        };
        let generation = self.invalidate_ink(sel.page, sel.bounds);
        let page_size = {
            let doc = imp.doc.borrow();
            doc.as_ref()
                .and_then(|d| Some(d.pages[d.page_index(sel.page)?].1))
                .map(|spec| (spec.width, spec.height))
        };
        if let (Some(ink), Some(page_size)) = (sel.texture, page_size) {
            imp.ghosts.borrow_mut().push(Ghost {
                page: sel.page,
                page_size,
                ink,
                areas: sel.strokes.iter().map(|s| s.bounds()).collect(),
                scale: self.target_scale(),
                generation,
                born: Instant::now(),
            });
        }
        self.request_tiles();
        self.queue_draw();
        self.emit_by_name::<()>("selection-changed", &[]);
    }

    /// Commits a drag: the strokes move through the history, keeping their order.
    fn move_selection(&self, (dx, dy): (f32, f32)) {
        let imp = self.imp();
        let Some(note) = self.note() else { return };
        let mut sel = imp.selection.borrow_mut();
        let Some(s) = sel.as_mut() else { return };
        let moved: Vec<Arc<Stroke>> = s
            .strokes
            .iter()
            .map(|st| Arc::new(st.translated(dx, dy)))
            .collect();
        let replaced = {
            let mut note = note.borrow_mut();
            let crate::note::Note { history, doc, .. } = &mut *note;
            history.replace_strokes(
                doc,
                s.page,
                s.indices
                    .iter()
                    .copied()
                    .zip(moved.iter().cloned())
                    .collect(),
            )
        };
        if let Err(e) = replaced {
            tracing::warn!("cannot move the selection: {e}");
            return;
        }
        // The moved strokes stay hidden from the tiles, so no tile needs redrawing.
        s.strokes = moved;
        s.bounds = s.bounds.translated(dx, dy);
        if let Some(t) = s.texture.as_mut() {
            t.rect = t.rect.translated(dx, dy);
        }
        drop(sel);
        self.emit_by_name::<()>("changed", &[]);
    }

    pub fn delete_selection(&self) {
        let imp = self.imp();
        let Some(note) = self.note() else { return };
        let Some(sel) = imp.selection.take() else {
            return;
        };
        let removed = {
            let mut note = note.borrow_mut();
            let crate::note::Note { history, doc, .. } = &mut *note;
            history.remove_strokes(doc, sel.page, &sel.indices)
        };
        if let Err(e) = removed {
            tracing::warn!("cannot delete the selection: {e}");
        }
        // The tiles already leave these strokes out.
        self.queue_draw();
        self.emit_by_name::<()>("selection-changed", &[]);
        self.emit_by_name::<()>("changed", &[]);
    }

    /// Recolours the selection. Highlighter strokes keep their translucency.
    pub fn recolour_selection(&self, color: Rgba8) {
        let imp = self.imp();
        let Some(note) = self.note() else { return };
        let mut sel = imp.selection.borrow_mut();
        let Some(s) = sel.as_mut() else { return };
        let recoloured: Vec<Arc<Stroke>> = s
            .strokes
            .iter()
            .map(|st| {
                let c = match st.tool() {
                    Tool::Highlighter => Rgba8 {
                        a: st.color().a,
                        ..color
                    },
                    Tool::Pen => color,
                };
                Arc::new(st.with_color(c))
            })
            .collect();
        let replaced = {
            let mut note = note.borrow_mut();
            let crate::note::Note { history, doc, .. } = &mut *note;
            history.replace_strokes(
                doc,
                s.page,
                s.indices
                    .iter()
                    .copied()
                    .zip(recoloured.iter().cloned())
                    .collect(),
            )
        };
        if let Err(e) = replaced {
            tracing::warn!("cannot recolour the selection: {e}");
            return;
        }
        s.texture = render_strokes(&recoloured, s.bounds, self.target_scale().get());
        s.strokes = recoloured;
        drop(sel);
        self.queue_draw();
        self.emit_by_name::<()>("changed", &[]);
    }

    /// Moves the selection up or down the page's drawing order, through the history. The
    /// selection is drawn on top until it ends, so the new order shows once it is deselected.
    pub fn arrange_selection(&self, how: Arrange) {
        let imp = self.imp();
        let Some(note) = self.note() else { return };
        let mut sel = imp.selection.borrow_mut();
        let Some(s) = sel.as_mut() else { return };
        let result = {
            let mut note = note.borrow_mut();
            let crate::note::Note { history, doc, .. } = &mut *note;
            let Some(index) = doc.page_index(s.page) else {
                return;
            };
            let all = match doc.load_ink(index) {
                Ok(all) => all.to_vec(),
                Err(e) => {
                    tracing::warn!("cannot read ink: {e}");
                    return;
                }
            };
            // Highlighters always sit under pen ink, so only strokes of a selected tool count.
            let tools: Vec<Tool> = s.strokes.iter().map(|st| st.tool()).collect();
            let Some((order, moved)) = arrange(
                all.len(),
                &s.indices,
                |i| tools.contains(&all[i].tool()) && all[i].bounds().intersects(&s.bounds),
                how,
            ) else {
                return;
            };
            let changes = order
                .iter()
                .enumerate()
                .filter(|&(p, &i)| p != i)
                .map(|(p, &i)| (p, all[i].clone()))
                .collect();
            history
                .replace_strokes(doc, s.page, changes)
                .map(|()| moved)
        };
        match result {
            // The other strokes keep their order relative to each other, so their tiles stay.
            Ok(moved) => s.indices = moved,
            Err(e) => {
                tracing::warn!("cannot reorder the selection: {e}");
                return;
            }
        }
        drop(sel);
        self.emit_by_name::<()>("changed", &[]);
    }

    pub fn copy_selection(&self) -> bool {
        let sel = self.imp().selection.borrow();
        let Some(s) = sel.as_ref() else {
            return false;
        };
        CLIPBOARD.with(|c| *c.borrow_mut() = s.strokes.clone());
        true
    }

    pub fn cut_selection(&self) {
        if self.copy_selection() {
            self.delete_selection();
        }
    }

    /// Pastes the clipboard in the middle of the visible part of the current page and selects
    /// it. Returns whether there was anything to paste.
    pub fn paste(&self) -> bool {
        let strokes = CLIPBOARD.with(|c| c.borrow().clone());
        let Some(bounds) = strokes.iter().map(|s| s.bounds()).reduce(Rect::union) else {
            return false;
        };
        let imp = self.imp();
        let Some(note) = self.note() else {
            return false;
        };
        let Some(view) = self.view() else {
            return false;
        };
        self.clear_selection();
        let (index, page, spec, rect) = {
            let doc = imp.doc.borrow();
            let Some(doc) = doc.as_ref() else {
                return false;
            };
            let index = (imp.current_page.get() as usize).min(doc.pages.len().saturating_sub(1));
            let (page, spec) = doc.pages[index];
            (index, page, spec, doc.layout.page_rect(index))
        };
        // Centre of the visible part of the page, kept on the page.
        let (vx0, vy0, vx1, vy1) = view.visible_pt();
        let cx = ((vx0.max(rect.x) + vx1.min(rect.x + rect.w)) / 2.0 - rect.x) as f32;
        let cy = ((vy0.max(rect.y) + vy1.min(rect.y + rect.h)) / 2.0 - rect.y) as f32;
        let (w, h) = (bounds.x1 - bounds.x0, bounds.y1 - bounds.y0);
        let x0 = (cx - w / 2.0).clamp(0.0, (spec.width - w).max(0.0));
        let y0 = (cy - h / 2.0).clamp(0.0, (spec.height - h).max(0.0));
        let (dx, dy) = (x0 - bounds.x0, y0 - bounds.y0);
        let pasted: Vec<Arc<Stroke>> = strokes
            .iter()
            .map(|s| Arc::new(s.translated(dx, dy)))
            .collect();
        let added = {
            let mut note = note.borrow_mut();
            let crate::note::Note { history, doc, .. } = &mut *note;
            let first = doc.load_ink(index).map(<[_]>::len);
            first.and_then(|first| {
                history
                    .add_strokes(doc, page, pasted.clone())
                    .map(|()| first..first + pasted.len())
            })
        };
        match added {
            Ok(indices) => {
                self.select(page, indices.collect());
                self.emit_by_name::<()>("changed", &[]);
                true
            }
            Err(e) => {
                tracing::warn!("cannot paste: {e}");
                false
            }
        }
    }

    /// After the zoom settles: a sharp texture for the new scale.
    pub(super) fn refresh_selection_texture(&self) {
        let scale = self.target_scale().get();
        if let Some(s) = self.imp().selection.borrow_mut().as_mut() {
            s.texture = render_strokes(&s.strokes, s.bounds, scale);
        }
    }

    /// The selection's strokes if they are on `page`: the tiles leave them out.
    pub(super) fn hidden_on(&self, page: PageId) -> Vec<Arc<Stroke>> {
        self.imp()
            .selection
            .borrow()
            .as_ref()
            .filter(|s| s.page == page)
            .map(|s| s.strokes.clone())
            .unwrap_or_default()
    }

    /// Ghosts, the selection and the lasso loop on one page. `page` is its view rectangle.
    pub(super) fn draw_lasso_layer(
        &self,
        snapshot: &gtk::Snapshot,
        cache: &TileCache<gdk::Texture>,
        doc: DocId,
        id: PageId,
        page: &graphene::Rect,
        zoom: f64,
    ) {
        let imp = self.imp();
        let to_view = |r: Rect| {
            graphene::Rect::new(
                page.x() + (f64::from(r.x0) * zoom) as f32,
                page.y() + (f64::from(r.y0) * zoom) as f32,
                (f64::from(r.x1 - r.x0) * zoom) as f32,
                (f64::from(r.y1 - r.y0) * zoom) as f32,
            )
        };
        for g in imp.ghosts.borrow().iter().filter(|g| g.page == id) {
            if !ghost_caught_up(cache, doc, g) {
                snapshot.append_texture(&g.ink.texture, &to_view(g.ink.rect));
            }
        }
        if let Some(s) = imp.selection.borrow().as_ref().filter(|s| s.page == id) {
            let (dx, dy) = s.offset;
            if let Some(t) = &s.texture {
                snapshot.append_texture(&t.texture, &to_view(t.rect.translated(dx, dy)));
            }
            let frame = to_view(s.bounds.translated(dx, dy).expanded((4.0 / zoom) as f32));
            let builder = gsk::PathBuilder::new();
            builder.add_rect(&frame);
            let stroke = gsk::Stroke::new(1.5);
            stroke.set_dash(&[6.0, 4.0]);
            snapshot.append_stroke(&builder.to_path(), &stroke, &accent());
        }
        if let Some(path) = imp.lasso.borrow().as_ref().filter(|p| p.page == id) {
            let builder = gsk::PathBuilder::new();
            for (i, &(x, y)) in path.points.iter().enumerate() {
                let (vx, vy) = (
                    page.x() + (f64::from(x) * zoom) as f32,
                    page.y() + (f64::from(y) * zoom) as f32,
                );
                if i == 0 {
                    builder.move_to(vx, vy);
                } else {
                    builder.line_to(vx, vy);
                }
            }
            if path.rect {
                builder.close();
            }
            let stroke = gsk::Stroke::new(1.5);
            stroke.set_dash(&[4.0, 4.0]);
            snapshot.append_stroke(&builder.to_path(), &stroke, &accent());
        }
    }

    /// Drops ghosts whose tiles have caught up, that are off screen or from an old zoom.
    pub(super) fn prune_ghosts(
        &self,
        cache: &TileCache<gdk::Texture>,
        doc: DocId,
        on_screen: &[PageId],
    ) {
        let target = self.target_scale();
        self.imp().ghosts.borrow_mut().retain(|g| {
            g.scale == target
                && on_screen.contains(&g.page)
                && g.born.elapsed() < GHOST_LIFETIME
                && !ghost_caught_up(cache, doc, g)
        });
    }
}

/// Whether every ink tile the ghost's strokes reach is up to date for its generation. Those
/// are exactly the tiles the canvas asks for, since it asks for any tile a stroke's bounds
/// touch.
fn ghost_caught_up(cache: &TileCache<gdk::Texture>, doc: DocId, g: &Ghost) -> bool {
    let grid = Grid::new(g.page_size, g.scale);
    let s = g.scale.get();
    let r = g.ink.rect;
    let tiles = grid.tiles_in(
        f64::from(r.x0) * s,
        f64::from(r.y0) * s,
        f64::from(r.x1) * s,
        f64::from(r.y1) * s,
    );
    tiles
        .filter(|&(tx, ty)| {
            let area = tile_area(grid.tile_rect(tx, ty), g.scale);
            g.areas.iter().any(|a| a.intersects(&area))
        })
        .all(|(tx, ty)| {
            let key = TileKey {
                doc,
                page: g.page,
                layer: Layer::Ink,
                scale: g.scale,
                tx,
                ty,
            };
            cache
                .fresh_generation(&key)
                .is_some_and(|f| f >= g.generation)
        })
}

/// Draws strokes into one texture at `scale` device pixels per point (less for big ones).
fn render_strokes(strokes: &[Arc<Stroke>], bounds: Rect, scale: f64) -> Option<InkTexture> {
    let r = bounds.expanded(1.0);
    let (w, h) = (f64::from(r.x1 - r.x0), f64::from(r.y1 - r.y0));
    let s = scale.min(MAX_TEXTURE_PX / w.max(h).max(1.0));
    let (pw, ph) = (
        (w * s).ceil().max(1.0) as i32,
        (h * s).ceil().max(1.0) as i32,
    );
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, pw, ph).ok()?;
    {
        let cr = cairo::Context::new(&surface).ok()?;
        cr.scale(s, s);
        cr.translate(-f64::from(r.x0), -f64::from(r.y0));
        ink::draw_ink(&cr, strokes, None);
    }
    surface.flush();
    let stride = surface.stride() as usize;
    let bytes = glib::Bytes::from_owned(surface.take_data().ok()?);
    let texture = gdk::MemoryTexture::new(pw, ph, PIXEL_FORMAT, &bytes, stride).upcast();
    // The texture's own extent, so it isn't stretched.
    let rect = Rect {
        x1: r.x0 + (f64::from(pw) / s) as f32,
        y1: r.y0 + (f64::from(ph) / s) as f32,
        ..r
    };
    Some(InkTexture { texture, rect })
}
