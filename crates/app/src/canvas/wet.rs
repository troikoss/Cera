// SPDX-License-Identifier: GPL-3.0-or-later
//! Wet ink: the stroke being drawn, rasterised as it grows (DESIGN.md §5).
//!
//! It uses the same tile grid as the finished ink tiles. Each motion event clears the small
//! pixel-aligned box its new samples touch and redraws the stroke's shapes there in one fill,
//! with the same geometry as the render workers, so a wet tile and the finished tile that
//! replaces it are the same pixels. After pen-up the stroke stays until, tile by tile, the
//! finished tiles for its generation arrive.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use cera_core::{PageId, Point, Rect, Rgba8, StrokeStyle};
use cera_render::ink;
use cera_render::tile::{Grid, PixelRect, Scale};
use gtk::prelude::*;
use gtk::{cairo, gdk, glib};

use super::PIXEL_FORMAT;

struct WetTile {
    surface: cairo::ImageSurface,
    rect: PixelRect,
    texture: Option<gdk::Texture>,
    dirty: bool,
}

/// How the stroke is drawn.
#[derive(Clone, Copy)]
pub struct Pen {
    pub style: StrokeStyle,
    pub color: Rgba8,
    pub width: f32,
    /// Pressure is off: every sample is at full pressure, so a round stroke is drawn as a line
    /// as the finished stroke will be (`ink::is_constant`).
    pub constant: bool,
}

pub struct WetStroke {
    pub page: PageId,
    pub pen: Pen,
    pub scale: Scale,
    grid: Grid,
    /// Samples in page points.
    pub points: Vec<Point>,
    tiles: HashMap<(u16, u16), WetTile>,
    /// Set at pen-up: the page's ink generation that includes this stroke.
    pub generation: Option<u64>,
}

impl WetStroke {
    pub fn new(page: PageId, page_size: (f32, f32), pen: Pen, scale: Scale) -> Self {
        Self {
            page,
            pen,
            scale,
            grid: Grid::new(page_size, scale),
            points: Vec::new(),
            tiles: HashMap::new(),
            generation: None,
        }
    }

    /// Adds samples and redraws the area they (and the previous sample) touch.
    pub fn extend(&mut self, samples: &[Point]) {
        if samples.is_empty() {
            return;
        }
        let from = self.points.len().saturating_sub(1);
        if self.pen.constant {
            let full = samples.iter().map(|p| Point {
                pressure: 1.0,
                ..*p
            });
            self.points.extend(full);
        } else {
            self.points.extend_from_slice(samples);
        }
        let Self {
            pen,
            scale,
            grid,
            points,
            tiles,
            ..
        } = self;
        let s = scale.get();
        let reach = |p: &Point| f64::from(ink::reach(pen.style, pen.width, p.pressure));
        // Box of the new shapes in page pixels, with a pixel of margin for antialiasing.
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for p in &points[from..] {
            let r = reach(p) * s + 1.0;
            let (x, y) = (f64::from(p.x) * s, f64::from(p.y) * s);
            (x0, y0, x1, y1) = (x0.min(x - r), y0.min(y - r), x1.max(x + r), y1.max(y + r));
        }
        let (x0, y0) = (x0.floor().max(0.0), y0.floor().max(0.0));
        let x1 = x1.ceil().min(f64::from(grid.width));
        let y1 = y1.ceil().min(f64::from(grid.height));
        if x0 >= x1 || y0 >= y1 {
            return; // entirely off the page
        }
        let area = Rect {
            x0: (x0 / s) as f32,
            y0: (y0 / s) as f32,
            x1: (x1 / s) as f32,
            y1: (y1 / s) as f32,
        };
        for (tx, ty) in grid.tiles_in(x0, y0, x1, y1) {
            let rect = grid.tile_rect(tx, ty);
            let tile = match tiles.entry((tx, ty)) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => {
                    let format = cairo::Format::ARgb32;
                    let size = (rect.width as i32, rect.height as i32);
                    let Ok(surface) = cairo::ImageSurface::create(format, size.0, size.1) else {
                        continue;
                    };
                    e.insert(WetTile {
                        surface,
                        rect,
                        texture: None,
                        dirty: false,
                    })
                }
            };
            if let Err(e) = redraw(tile, points, area, (x0, y0, x1, y1), *pen, s) {
                tracing::warn!("wet ink: {e}");
            }
            tile.dirty = true;
        }
    }

    /// Turns tiles redrawn since the last call into textures. Called once per input event,
    /// which GTK limits to about one per frame, so `snapshot` only appends textures.
    pub fn flush(&mut self) {
        for tile in self.tiles.values_mut().filter(|t| t.dirty) {
            tile.dirty = false;
            let stride = tile.surface.stride() as usize;
            let mut bytes = None;
            let _ = tile
                .surface
                .with_data(|data| bytes = Some(glib::Bytes::from(data)));
            if let Some(bytes) = bytes {
                let texture = gdk::MemoryTexture::new(
                    tile.rect.width as i32,
                    tile.rect.height as i32,
                    PIXEL_FORMAT,
                    &bytes,
                    stride,
                );
                tile.texture = Some(texture.upcast());
            }
        }
    }

    /// Tiles with their pixel rectangles in the page grid.
    pub fn tiles(&self) -> impl Iterator<Item = ((u16, u16), PixelRect, &gdk::Texture)> {
        self.tiles
            .iter()
            .filter_map(|(&k, t)| Some((k, t.rect, t.texture.as_ref()?)))
    }

    /// Wet tiles are drawn opaque; the stroke's alpha is applied as a group opacity, which is
    /// what a single translucent fill looks like.
    pub fn opacity(&self) -> f32 {
        f32::from(self.pen.color.a) / 255.0
    }

    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }

    /// Forgets a tile whose ink was all erased or undone: no finished tile will come to
    /// replace it.
    pub fn drop_tile(&mut self, t: (u16, u16)) {
        self.tiles.remove(&t);
    }
}

/// Clears the pixel box in one tile and redraws the stroke's shapes there in one fill.
fn redraw(
    tile: &WetTile,
    points: &[Point],
    area: Rect,
    (x0, y0, x1, y1): (f64, f64, f64, f64),
    pen: Pen,
    scale: f64,
) -> Result<(), cairo::Error> {
    let cr = cairo::Context::new(&tile.surface)?;
    cr.translate(-f64::from(tile.rect.x), -f64::from(tile.rect.y));
    cr.rectangle(x0, y0, x1 - x0, y1 - y0);
    cr.clip();
    cr.set_operator(cairo::Operator::Clear);
    cr.paint()?;
    cr.set_operator(cairo::Operator::Over);
    cr.scale(scale, scale);
    ink::set_color(&cr, pen.color, true);
    ink::paint_shape(&cr, pen.style, pen.constant, pen.width, points, Some(area))
}
