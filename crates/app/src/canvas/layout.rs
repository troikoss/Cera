// SPDX-License-Identifier: GPL-3.0-or-later
//! Where pages sit: a vertical stack in points, centred, scaled linearly by the zoom.
//!
//! View coordinates are logical pixels relative to the canvas widget:
//! `view = pt * zoom + offset - scroll`, where `offset.x` centres a stack narrower than the
//! widget. Gaps are in points too, so the whole layout scales with the zoom and zooming
//! around a point is exact.

use std::ops::Range;

/// Gap between pages and around the stack, in points.
pub const GAP_PT: f64 = 12.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RectPt {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Clone, Debug)]
pub struct Layout {
    sizes: Vec<(f64, f64)>,
    /// Top and bottom of each page in points; both increase with the index.
    tops: Vec<f64>,
    bottoms: Vec<f64>,
    width: f64,
    height: f64,
}

impl Layout {
    pub fn new(sizes: impl IntoIterator<Item = (f32, f32)>) -> Self {
        let sizes: Vec<(f64, f64)> = sizes
            .into_iter()
            .map(|(w, h)| (f64::from(w), f64::from(h)))
            .collect();
        let mut tops = Vec::with_capacity(sizes.len());
        let mut bottoms = Vec::with_capacity(sizes.len());
        let mut y = GAP_PT;
        for &(_, h) in &sizes {
            tops.push(y);
            bottoms.push(y + h);
            y += h + GAP_PT;
        }
        let widest = sizes.iter().map(|s| s.0).fold(0.0, f64::max);
        Self {
            width: widest + 2.0 * GAP_PT,
            height: y,
            sizes,
            tops,
            bottoms,
        }
    }

    pub fn len(&self) -> usize {
        self.sizes.len()
    }

    /// Stack size in points, gaps included.
    pub fn width(&self) -> f64 {
        self.width
    }

    pub fn height(&self) -> f64 {
        self.height
    }

    pub fn page_rect(&self, index: usize) -> RectPt {
        let (w, h) = self.sizes[index];
        RectPt {
            x: (self.width - w) / 2.0,
            y: self.tops[index],
            w,
            h,
        }
    }

    /// Pages overlapping the band `[y0, y1]` in points.
    pub fn pages_between(&self, y0: f64, y1: f64) -> Range<usize> {
        let start = self.bottoms.partition_point(|&bottom| bottom < y0);
        let end = self.tops.partition_point(|&top| top <= y1);
        start..end.max(start)
    }

    /// The page nearest to `y` (points); 0 for an empty layout.
    pub fn page_at(&self, y: f64) -> usize {
        let i = self.tops.partition_point(|&top| top <= y);
        i.saturating_sub(1)
    }
}

/// How the stack maps onto the widget.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub zoom: f64,
    pub scroll_x: f64,
    pub scroll_y: f64,
    pub width: f64,
    pub height: f64,
    /// Horizontal centring offset in logical pixels.
    pub offset_x: f64,
}

impl View {
    pub fn new(layout: &Layout, zoom: f64, scroll: (f64, f64), size: (f64, f64)) -> Self {
        Self {
            zoom,
            scroll_x: scroll.0,
            scroll_y: scroll.1,
            width: size.0,
            height: size.1,
            offset_x: centring(layout, zoom, size.0),
        }
    }

    pub fn to_view(self, x: f64, y: f64) -> (f64, f64) {
        (
            x * self.zoom + self.offset_x - self.scroll_x,
            y * self.zoom - self.scroll_y,
        )
    }

    pub fn to_pt(self, vx: f64, vy: f64) -> (f64, f64) {
        (
            (vx + self.scroll_x - self.offset_x) / self.zoom,
            (vy + self.scroll_y) / self.zoom,
        )
    }

    /// The visible band in points, `(x0, y0, x1, y1)`.
    pub fn visible_pt(self) -> (f64, f64, f64, f64) {
        let (x0, y0) = self.to_pt(0.0, 0.0);
        let (x1, y1) = self.to_pt(self.width, self.height);
        (x0, y0, x1, y1)
    }
}

fn centring(layout: &Layout, zoom: f64, widget_width: f64) -> f64 {
    ((widget_width - layout.width() * zoom) / 2.0).max(0.0)
}

/// Zoom that fits the stack's width to the widget.
pub fn fit_width_zoom(layout: &Layout, widget_width: f64) -> f64 {
    if layout.width() > 0.0 {
        widget_width / layout.width()
    } else {
        1.0
    }
}

/// Scroll position that keeps point `anchor_pt` under view position `anchor_view` at `zoom`.
pub fn scroll_for_anchor(
    layout: &Layout,
    zoom: f64,
    widget_width: f64,
    anchor_pt: (f64, f64),
    anchor_view: (f64, f64),
) -> (f64, f64) {
    (
        anchor_pt.0 * zoom + centring(layout, zoom, widget_width) - anchor_view.0,
        anchor_pt.1 * zoom - anchor_view.1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout::new([(612.0, 792.0), (595.0, 842.0), (842.0, 595.0)])
    }

    #[test]
    fn pages_stack_with_gaps_and_centre() {
        let l = layout();
        assert_eq!(l.width(), 842.0 + 2.0 * GAP_PT);
        assert_eq!(l.height(), 792.0 + 842.0 + 595.0 + 4.0 * GAP_PT);
        let p1 = l.page_rect(1);
        assert_eq!(p1.y, 792.0 + 2.0 * GAP_PT);
        assert_eq!(p1.x, (l.width() - 595.0) / 2.0);
    }

    #[test]
    fn visible_pages() {
        let l = layout();
        assert_eq!(l.pages_between(0.0, 5.0), 0..0, "only the top gap");
        assert_eq!(l.pages_between(0.0, 20.0), 0..1);
        assert_eq!(l.pages_between(700.0, 900.0), 0..2);
        let gap_top = 792.0 + GAP_PT + 1.0;
        assert_eq!(
            l.pages_between(gap_top, gap_top + 2.0),
            1..1,
            "between pages"
        );
        assert_eq!(l.pages_between(0.0, 1e9), 0..3);
        assert_eq!(l.pages_between(1e8, 1e9), 3..3);
        assert_eq!(l.page_at(0.0), 0);
        assert_eq!(l.page_at(900.0), 1);
        assert_eq!(l.page_at(1e9), 2);
    }

    #[test]
    fn zoom_keeps_the_anchor_in_place() {
        let l = layout();
        let before = View::new(&l, 1.5, (100.0, 400.0), (800.0, 600.0));
        let anchor_view = (300.0, 250.0);
        let anchor_pt = before.to_pt(anchor_view.0, anchor_view.1);
        let scroll = scroll_for_anchor(&l, 3.0, 800.0, anchor_pt, anchor_view);
        let after = View::new(&l, 3.0, scroll, (800.0, 600.0));
        let (vx, vy) = after.to_view(anchor_pt.0, anchor_pt.1);
        assert!((vx - anchor_view.0).abs() < 1e-9 && (vy - anchor_view.1).abs() < 1e-9);
    }

    #[test]
    fn narrow_stacks_are_centred() {
        let l = layout();
        let v = View::new(&l, 0.5, (0.0, 0.0), (1000.0, 600.0));
        assert_eq!(v.offset_x, (1000.0 - l.width() * 0.5) / 2.0);
        assert!((fit_width_zoom(&l, 1000.0) * l.width() - 1000.0).abs() < 1e-9);
    }
}
