//! Tile keys and grid maths, shared by the render worker and the canvas.
//!
//! A page at a given scale is a pixel grid cut into 256 × 256 tiles (edge tiles are smaller).
//! Small pages, as at low zoom, are a single texture instead: fewer nodes per frame, and a
//! scanned page decodes its image once instead of once per tile.

use cera_core::PageId;

/// Tile edge in device pixels.
pub const TILE_PX: u32 = 256;
/// Pages whose longer side is at most this many device pixels render as one texture.
pub const WHOLE_PAGE_PX: u32 = 1024;

/// Identifies an open note within one renderer.
pub type DocId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Layer {
    Pdf,
    // Ink arrives in M4.
}

/// Device pixels per point. Stored as bits so it can be part of a hashed key; tiles are
/// rendered at exactly the settled zoom, so equal scales compare equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Scale(u32);

impl Scale {
    pub fn new(px_per_pt: f64) -> Self {
        Self((px_per_pt as f32).to_bits())
    }

    pub fn get(self) -> f64 {
        f64::from(f32::from_bits(self.0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub doc: DocId,
    pub page: PageId,
    pub layer: Layer,
    pub scale: Scale,
    pub tx: u16,
    pub ty: u16,
}

/// A rectangle in a page's pixel grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PixelRect {
    pub fn union(self, o: PixelRect) -> PixelRect {
        let (x, y) = (self.x.min(o.x), self.y.min(o.y));
        let x1 = (self.x + self.width).max(o.x + o.width);
        let y1 = (self.y + self.height).max(o.y + o.height);
        PixelRect {
            x,
            y,
            width: x1 - x,
            height: y1 - y,
        }
    }

    pub fn area(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

/// The pixel grid of one page at one scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub width: u32,
    pub height: u32,
    pub tile: u32,
}

impl Grid {
    pub fn new((width_pt, height_pt): (f32, f32), scale: Scale) -> Self {
        let px = |pt: f32| {
            (f64::from(pt) * scale.get())
                .ceil()
                .clamp(1.0, f64::from(u32::MAX / 2)) as u32
        };
        let (width, height) = (px(width_pt), px(height_pt));
        let tile = if width.max(height) <= WHOLE_PAGE_PX {
            width.max(height)
        } else {
            TILE_PX
        };
        Self {
            width,
            height,
            tile,
        }
    }

    pub fn cols(&self) -> u32 {
        self.width.div_ceil(self.tile)
    }

    pub fn rows(&self) -> u32 {
        self.height.div_ceil(self.tile)
    }

    /// Pixels covered by tile (tx, ty), clipped to the page.
    pub fn tile_rect(&self, tx: u16, ty: u16) -> PixelRect {
        let (x, y) = (u32::from(tx) * self.tile, u32::from(ty) * self.tile);
        PixelRect {
            x,
            y,
            width: self.tile.min(self.width.saturating_sub(x)),
            height: self.tile.min(self.height.saturating_sub(y)),
        }
    }

    /// Tiles overlapping the pixel rectangle `[x0, x1) × [y0, y1)`, which may extend past
    /// the page; row by row.
    pub fn tiles_in(
        &self,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
    ) -> impl Iterator<Item = (u16, u16)> + use<> {
        let tile = f64::from(self.tile);
        let range = |a: f64, b: f64, n: u32| {
            let first = (a / tile).floor().max(0.0) as u32;
            let end = ((b / tile).ceil().max(0.0) as u32).min(n);
            first..end.max(first)
        };
        let cols = range(x0, x1, self.cols());
        range(y0, y1, self.rows())
            .flat_map(move |ty| cols.clone().map(move |tx| (tx as u16, ty as u16)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_pages_are_one_tile() {
        let g = Grid::new((612.0, 792.0), Scale::new(1.0));
        assert_eq!((g.cols(), g.rows(), g.tile), (1, 1, 792));
        assert_eq!(
            g.tile_rect(0, 0),
            PixelRect {
                x: 0,
                y: 0,
                width: 612,
                height: 792
            }
        );
    }

    #[test]
    fn large_pages_are_cut_into_tiles_with_clipped_edges() {
        // Letter at fit width on the tablet: ~4.0 device px per pt.
        let g = Grid::new((612.0, 792.0), Scale::new(4.0));
        assert_eq!((g.width, g.height), (2448, 3168));
        assert_eq!((g.cols(), g.rows()), (10, 13));
        assert_eq!(
            g.tile_rect(9, 12),
            PixelRect {
                x: 2304,
                y: 3072,
                width: 144,
                height: 96
            }
        );
    }

    #[test]
    fn tiles_in_clamps_to_the_page() {
        let g = Grid::new((612.0, 792.0), Scale::new(4.0));
        let all: Vec<_> = g.tiles_in(-100.0, -100.0, 1e9, 1e9).collect();
        assert_eq!(all.len(), 130);
        let some: Vec<_> = g.tiles_in(250.0, 0.0, 513.0, 10.0).collect();
        assert_eq!(some, [(0, 0), (1, 0), (2, 0)]);
        assert_eq!(g.tiles_in(-50.0, -50.0, -1.0, -1.0).count(), 0);
        assert_eq!(g.tiles_in(5000.0, 0.0, 6000.0, 10.0).count(), 0);
    }
}
