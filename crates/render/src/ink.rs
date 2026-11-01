// SPDX-License-Identifier: GPL-3.0-or-later
//! Stroke geometry and drawing. The render workers (finished ink tiles) and the canvas (wet
//! ink while writing) both draw with these functions, so the stroke doesn't change when the
//! finished tile replaces the wet one.
//!
//! A pen stroke is the union of a disc at every point, its radius from the pressure, and the
//! hull between neighbouring discs, filled once (no overlapping alpha, round ends for free).
//! Finished strokes skip the discs where the stroke barely turns: there the hulls already meet
//! to within a thousandth of a pixel, and filling hundreds of discs is what costs time.
//! A round highlight (under or over the ink) at full pressure throughout is a constant-width
//! polyline with round joins and caps instead, as highlighters always were. Ink keeps the pen
//! outline even at full pressure, so notes from before stroke styles draw the same pixels.
//! A stroke with a square or rounded-square nib is the union of the convex hulls of each
//! pair of neighbouring nibs, also filled once.
//!
//! Each layer is drawn in turn: highlights under the ink, the ink, highlights over it.

use std::cell::RefCell;
use std::f64::consts::{FRAC_PI_2, TAU};
use std::sync::Arc;

use cera_core::{Layer, Point, Rect, Rgba8, Stroke, StrokeStyle};

/// Radius at zero pressure, as a fraction of the full-pressure radius.
const MIN_RADIUS: f64 = 0.25;
/// Interior discs are drawn where the hull edges turn by more than this (radians, ~4°).
const DISC_TURN: f64 = 0.07;

/// Pen radius in points for a point's pressure; `width` is the full-pressure width.
pub fn pen_radius(width: f32, pressure: f32) -> f64 {
    let p = f64::from(pressure.clamp(0.0, 1.0));
    f64::from(width) / 2.0 * (MIN_RADIUS + (1.0 - MIN_RADIUS) * p)
}

/// The quadrilateral between the outer tangents of two discs, wound like cairo's arcs so the
/// union fills under the winding rule. `None` if one disc contains the other.
pub fn hull(a: (f64, f64), ra: f64, b: (f64, f64), rb: f64) -> Option<[(f64, f64); 4]> {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let d = (dx * dx + dy * dy).sqrt();
    if d <= (ra - rb).abs() || d == 0.0 {
        return None;
    }
    let (ux, uy) = (dx / d, dy / d);
    let (nx, ny) = (-uy, ux);
    // Tangent points sit at angle β from the normal, towards the smaller disc.
    let sin = (ra - rb) / d;
    let cos = (1.0 - sin * sin).sqrt();
    let at = |c: (f64, f64), r: f64, side: f64| {
        (
            c.0 + r * (ux * sin + side * nx * cos),
            c.1 + r * (uy * sin + side * ny * cos),
        )
    };
    let mut quad = [
        at(a, ra, 1.0),
        at(b, rb, 1.0),
        at(b, rb, -1.0),
        at(a, ra, -1.0),
    ];
    if signed_area(&quad) < 0.0 {
        quad.reverse();
    }
    Some(quad)
}

fn signed_area(poly: &[(f64, f64)]) -> f64 {
    let n = poly.len();
    (0..n)
        .map(|i| {
            let (a, b) = (poly[i], poly[(i + 1) % n]);
            a.0 * b.1 - b.0 * a.1
        })
        .sum()
}

fn xy(p: Point) -> (f64, f64) {
    (f64::from(p.x), f64::from(p.y))
}

fn pen_hull(width: f32, a: Point, b: Point) -> Option<[(f64, f64); 4]> {
    hull(
        xy(a),
        pen_radius(width, a.pressure),
        xy(b),
        pen_radius(width, b.pressure),
    )
}

/// Box of the disc of radius `r` around `p`.
fn disc_box(p: Point, r: f64) -> Rect {
    let r = r as f32;
    Rect {
        x0: p.x - r,
        y0: p.y - r,
        x1: p.x + r,
        y1: p.y + r,
    }
}

fn add_quad(cr: &cairo::Context, q: &[(f64, f64); 4]) {
    cr.move_to(q[0].0, q[0].1);
    for p in &q[1..] {
        cr.line_to(p.0, p.1);
    }
    cr.close_path();
}

/// Adds a pen stroke's shape: every hull, and discs at the ends and where the outline turns.
/// With `area`, only the shapes that can paint inside it, which is how wet ink redraws the
/// region a new sample touched; the last point counts as an end either way.
pub fn add_pen_path(cr: &cairo::Context, width: f32, points: &[Point], area: Option<Rect>) {
    let hulls: Vec<_> = points
        .windows(2)
        .map(|w| pen_hull(width, w[0], w[1]))
        .collect();
    let touches = |r: Rect| area.is_none_or(|a| a.intersects(&r));
    let radius = |p: Point| pen_radius(width, p.pressure);
    for (i, &p) in points.iter().enumerate() {
        let before = i.checked_sub(1).map(|j| hulls[j]);
        let after = hulls.get(i).copied();
        let needs_disc = match (before, after) {
            (Some(Some(b)), Some(Some(a))) => edge_turn(&b, &a) > DISC_TURN,
            _ => true, // an end, or a disc that contains its neighbour
        };
        if needs_disc && touches(disc_box(p, radius(p))) {
            let (x, y) = xy(p);
            cr.new_sub_path();
            cr.arc(x, y, radius(p), 0.0, TAU);
            cr.close_path();
        }
    }
    for (w, q) in points.windows(2).zip(&hulls) {
        let Some(q) = q else { continue };
        if touches(disc_box(w[0], radius(w[0])).union(disc_box(w[1], radius(w[1])))) {
            add_quad(cr, q);
        }
    }
}

/// How much the outline turns where hull `b` (ending at a point) meets hull `a` (starting
/// there): the larger angle between their matching side edges.
fn edge_turn(b: &[(f64, f64); 4], a: &[(f64, f64); 4]) -> f64 {
    let angle = |p: (f64, f64), q: (f64, f64)| (q.1 - p.1).atan2(q.0 - p.0);
    let diff = |x: f64, y: f64| {
        let d = (x - y).abs() % TAU;
        d.min(TAU - d)
    };
    // Corners 0→1 and 3→2 are the two sides, both running from the first point to the second.
    let sides = |h: &[(f64, f64); 4]| [angle(h[0], h[1]), angle(h[3], h[2])];
    let (sb, sa) = (sides(b), sides(a));
    // `hull` may have reversed either quad, swapping which side comes first.
    let same = diff(sb[0], sa[0]).max(diff(sb[1], sa[1]));
    let swapped = diff(sb[0], sa[1]).max(diff(sb[1], sa[0]));
    same.min(swapped)
}

/// Adds a highlighter's centre line; with `area`, only the runs of segments that can paint
/// inside it (a run's end lies where the real stroke has a round join of the same size).
pub fn add_highlighter_path(cr: &cairo::Context, width: f32, points: &[Point], area: Option<Rect>) {
    let r = f64::from(width) / 2.0;
    let touches = |a: Point, b: Point| {
        area.is_none_or(|area| area.intersects(&disc_box(a, r).union(disc_box(b, r))))
    };
    if points.len() == 1 {
        if touches(points[0], points[0]) {
            let (x, y) = xy(points[0]);
            cr.move_to(x, y);
            cr.line_to(x, y);
        }
        return;
    }
    let mut open = false;
    for w in points.windows(2) {
        if touches(w[0], w[1]) {
            if !open {
                let (x, y) = xy(w[0]);
                cr.move_to(x, y);
                open = true;
            }
            let (x, y) = xy(w[1]);
            cr.line_to(x, y);
        } else {
            open = false;
        }
    }
}

pub fn set_highlighter_line(cr: &cairo::Context, width: f32) {
    cr.set_line_width(f64::from(width));
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
}

pub fn set_color(cr: &cairo::Context, c: Rgba8, opaque: bool) {
    let alpha = if opaque { 1.0 } else { f64::from(c.a) / 255.0 };
    cr.set_source_rgba(
        f64::from(c.r) / 255.0,
        f64::from(c.g) / 255.0,
        f64::from(c.b) / 255.0,
        alpha,
    );
}

/// Corner points per quarter circle of a rounded-square nib (4 segments: under half a pixel
/// off a 40 px corner).
const CORNER_POINTS: usize = 5;

/// How far a stroke's ink reaches from a point of it, in points.
pub fn reach(style: StrokeStyle, width: f32, pressure: f32) -> f32 {
    pen_radius(width, pressure) as f32 * style.reach_factor()
}

/// Whether every point is at full pressure: such a round highlight is drawn as a line.
pub fn is_constant(points: &[Point]) -> bool {
    points.iter().all(|p| p.pressure >= 1.0)
}

/// Adds a stroke's shape to the path and paints it with the current source. `constant` says
/// every point is at full pressure ([`is_constant`]); with `area`, only the shapes that can
/// paint inside it.
pub fn paint_shape(
    cr: &cairo::Context,
    style: StrokeStyle,
    constant: bool,
    width: f32,
    points: &[Point],
    area: Option<Rect>,
) -> Result<(), cairo::Error> {
    cr.new_path();
    if style.is_round() && constant && style.layer != Layer::Ink {
        set_highlighter_line(cr, width);
        add_highlighter_path(cr, width, points, area);
        return cr.stroke();
    }
    cr.set_fill_rule(cairo::FillRule::Winding);
    if style.is_round() {
        add_pen_path(cr, width, points, area);
    } else {
        add_nib_path(cr, style.roundness(), width, points, area);
    }
    cr.fill()
}

/// Draws a finished stroke in page points; with `area`, only the parts that paint inside it.
pub fn draw_stroke(cr: &cairo::Context, stroke: &Stroke, area: Option<Rect>) {
    set_color(cr, stroke.color(), false);
    let points = stroke.points();
    let constant = is_constant(points);
    let _ = paint_shape(cr, stroke.style(), constant, stroke.width(), points, area);
}

/// Draws strokes layer by layer (under, ink, over), each layer in page order. With `area`
/// (a tile), each stroke adds only its shapes that reach into it.
pub fn draw_ink(cr: &cairo::Context, strokes: &[Arc<Stroke>], area: Option<Rect>) {
    for layer in Layer::ALL {
        for stroke in strokes.iter().filter(|s| s.style().layer == layer) {
            draw_stroke(cr, stroke, area);
        }
    }
}

/// Adds the outline of a square or rounded-square nib stroke: the convex hull of each pair of
/// neighbouring nibs (just the nib for a single point). `roundness` is `0.0..1.0`; the nib
/// is aligned to the page axes and its radius follows the pressure like a pen's.
pub fn add_nib_path(
    cr: &cairo::Context,
    roundness: f32,
    width: f32,
    points: &[Point],
    area: Option<Rect>,
) {
    type Points = Vec<(f64, f64)>;
    // Reused corner and hull buffers: no allocation per segment.
    thread_local! {
        static BUF: RefCell<(Points, Points)> = const { RefCell::new((Vec::new(), Vec::new())) };
    }
    let radius = |p: Point| pen_radius(width, p.pressure);
    let touches = |r: Rect| area.is_none_or(|a| a.intersects(&r));
    let roundness = f64::from(roundness.clamp(0.0, 1.0));
    BUF.with_borrow_mut(|(corners, hull)| {
        let nib = |corners: &mut Vec<(f64, f64)>, p: Point| {
            nib_corners(corners, xy(p), radius(p), roundness);
        };
        if let [only] = points {
            if touches(disc_box(*only, radius(*only))) {
                corners.clear();
                nib(corners, *only);
                add_polygon(cr, corners);
            }
            return;
        }
        for w in points.windows(2) {
            let (a, b) = (w[0], w[1]);
            if !touches(disc_box(a, radius(a)).union(disc_box(b, radius(b)))) {
                continue;
            }
            corners.clear();
            nib(corners, a);
            nib(corners, b);
            convex_hull(corners, hull);
            add_polygon(cr, hull);
        }
    });
}

/// Appends the outline points of a nib at `c` with radius `r` (half its side): a square with
/// its corners rounded by `roundness * r`.
fn nib_corners(out: &mut Vec<(f64, f64)>, c: (f64, f64), r: f64, roundness: f64) {
    let corner = roundness * r;
    let inner = r - corner;
    for (i, (sx, sy)) in [(1.0, 1.0), (-1.0, 1.0), (-1.0, -1.0), (1.0, -1.0)]
        .into_iter()
        .enumerate()
    {
        let (cx, cy) = (c.0 + sx * inner, c.1 + sy * inner);
        if corner <= 0.0 {
            out.push((cx, cy));
            continue;
        }
        let start = i as f64 * FRAC_PI_2;
        for k in 0..CORNER_POINTS {
            let a = start + FRAC_PI_2 * k as f64 / (CORNER_POINTS - 1) as f64;
            out.push((cx + corner * a.cos(), cy + corner * a.sin()));
        }
    }
}

/// Andrew's monotone chain: the convex hull of `points` (sorted in place) into `out`, with a
/// positive signed area like [`hull`], so overlapping hulls add up under the winding rule.
fn convex_hull(points: &mut [(f64, f64)], out: &mut Vec<(f64, f64)>) {
    points.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    out.clear();
    let cross = |o: (f64, f64), a: (f64, f64), b: (f64, f64)| {
        (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
    };
    for pass in 0..2 {
        let start = out.len();
        let add = |p: (f64, f64), out: &mut Vec<(f64, f64)>| {
            while out.len() >= start + 2 && cross(out[out.len() - 2], out[out.len() - 1], p) <= 0.0
            {
                out.pop();
            }
            out.push(p);
        };
        if pass == 0 {
            points.iter().for_each(|&p| add(p, out));
        } else {
            points.iter().rev().for_each(|&p| add(p, out));
        }
        out.pop(); // the next chain starts with it
    }
}

fn add_polygon(cr: &cairo::Context, poly: &[(f64, f64)]) {
    let Some((&first, rest)) = poly.split_first() else {
        return;
    };
    cr.move_to(first.0, first.1);
    for p in rest {
        cr.line_to(p.0, p.1);
    }
    cr.close_path();
}

/// Whether a disc of `radius` around `p` (points) touches the stroke's ink.
pub fn hits(stroke: &Stroke, p: (f32, f32), radius: f32) -> bool {
    let probe = Rect {
        x0: p.0 - radius,
        y0: p.1 - radius,
        x1: p.0 + radius,
        y1: p.1 + radius,
    };
    if !stroke.bounds().intersects(&probe) {
        return false;
    }
    let reach = |q: &Point| radius + reach(stroke.style(), stroke.width(), q.pressure);
    let points = stroke.points();
    if let [only] = points {
        return distance_to_segment(p, *only, *only) <= reach(only);
    }
    points
        .windows(2)
        .any(|w| distance_to_segment(p, w[0], w[1]) <= reach(&w[0]).max(reach(&w[1])))
}

/// Whether a pen of `radius` dragged along `path` (points) touches the stroke's ink. Tested
/// segment against segment, so a fast drag that samples far apart still catches thin lines.
pub fn touches_path(stroke: &Stroke, path: &[(f32, f32)], radius: f32) -> bool {
    let Some((&first, rest)) = path.split_first() else {
        return false;
    };
    if rest.is_empty() {
        return hits(stroke, first, radius);
    }
    let b = stroke.bounds();
    let points = stroke.points();
    let reach = |q: &Point| radius + reach(stroke.style(), stroke.width(), q.pressure);
    let max_reach = points.iter().map(reach).fold(radius, f32::max);
    path.windows(2).any(|w| {
        let (p, q) = (w[0], w[1]);
        let probe = Rect {
            x0: p.0.min(q.0) - max_reach,
            y0: p.1.min(q.1) - max_reach,
            x1: p.0.max(q.0) + max_reach,
            y1: p.1.max(q.1) + max_reach,
        };
        if !b.intersects(&probe) {
            return false;
        }
        let seg = |a: Point, c: Point, r: f32| segments_distance(p, q, (a.x, a.y), (c.x, c.y)) <= r;
        match points {
            [only] => seg(*only, *only, reach(only)),
            _ => points
                .windows(2)
                .any(|s| seg(s[0], s[1], reach(&s[0]).max(reach(&s[1])))),
        }
    })
}

/// Shortest distance between segments `p`–`q` and `a`–`b`.
fn segments_distance(p: (f32, f32), q: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let cross = |o: (f32, f32), u: (f32, f32), v: (f32, f32)| {
        (u.0 - o.0) * (v.1 - o.1) - (u.1 - o.1) * (v.0 - o.0)
    };
    let (d1, d2) = (cross(p, q, a), cross(p, q, b));
    let (d3, d4) = (cross(a, b, p), cross(a, b, q));
    if d1 * d2 < 0.0 && d3 * d4 < 0.0 {
        return 0.0;
    }
    let pt = |x: (f32, f32)| Point {
        x: x.0,
        y: x.1,
        pressure: 0.0,
    };
    distance_to_segment(p, pt(a), pt(b))
        .min(distance_to_segment(q, pt(a), pt(b)))
        .min(distance_to_segment(a, pt(p), pt(q)))
        .min(distance_to_segment(b, pt(p), pt(q)))
}

fn distance_to_segment(p: (f32, f32), a: Point, b: Point) -> f32 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 {
        (((p.0 - a.x) * dx + (p.1 - a.y) * dy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    ((p.0 - a.x - t * dx).powi(2) + (p.1 - a.y - t * dy).powi(2)).sqrt()
}

/// The strokes that can paint inside `area`, in page order.
pub fn strokes_in(strokes: &[Arc<Stroke>], area: Rect) -> Vec<Arc<Stroke>> {
    strokes
        .iter()
        .filter(|s| s.bounds().intersects(&area))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hull_touches_both_discs_tangentially() {
        let (a, ra, b, rb) = ((0.0, 0.0), 2.0, (10.0, 3.0), 1.0);
        let q = hull(a, ra, b, rb).unwrap();
        let dist =
            |p: (f64, f64), c: (f64, f64)| ((p.0 - c.0).powi(2) + (p.1 - c.1).powi(2)).sqrt();
        // Two corners on each circle.
        let on_a = q
            .iter()
            .filter(|p| (dist(**p, a) - ra).abs() < 1e-9)
            .count();
        let on_b = q
            .iter()
            .filter(|p| (dist(**p, b) - rb).abs() < 1e-9)
            .count();
        assert_eq!((on_a, on_b), (2, 2));
        assert!(signed_area(&q) > 0.0);
        // The tangent line is perpendicular to the radius at its touching point.
        let (p, pb) = (q[0], q[1]);
        let (tx, ty) = (pb.0 - p.0, pb.1 - p.1);
        let r = if (dist(p, a) - ra).abs() < 1e-9 {
            (p.0 - a.0, p.1 - a.1)
        } else {
            (p.0 - b.0, p.1 - b.1)
        };
        assert!((tx * r.0 + ty * r.1).abs() < 1e-6);
        assert!(hull(a, 5.0, (1.0, 0.0), 1.0).is_none(), "contained disc");
    }

    fn render(stroke: Stroke) -> cairo::ImageSurface {
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 40, 40).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        draw_ink(&cr, &[Arc::new(stroke)], None);
        drop(cr);
        surface
    }

    fn alpha_at(surface: &mut cairo::ImageSurface, x: usize, y: usize) -> u8 {
        let stride = surface.stride() as usize;
        surface.data().unwrap()[y * stride + x * 4 + 3]
    }

    #[test]
    fn pen_fills_its_whole_shape_without_holes() {
        // Back-and-forth stroke: overlapping hulls in both directions must not cancel out.
        let pts = [(5.0, 20.0), (35.0, 20.0), (8.0, 21.0), (30.0, 19.0)].map(|(x, y)| Point {
            x,
            y,
            pressure: 1.0,
        });
        let mut s = render(Stroke::new(StrokeStyle::PEN, Rgba8::BLACK, 4.0, pts));
        for x in 6..30 {
            assert_eq!(alpha_at(&mut s, x, 20), 255, "hole at x={x}");
        }
        assert_eq!(alpha_at(&mut s, 20, 30), 0);
    }

    /// Wet ink redraws, in one fill, the pixel-aligned box each new sample touches. Once the
    /// stroke is finished, that must give the same pixels as drawing the stroke in one go.
    #[test]
    fn wet_ink_matches_the_finished_stroke() {
        let pts: Vec<_> = (0..120)
            .map(|i| {
                let t = i as f32 * 0.3;
                let pressure = 0.3 + (t / 7.0).sin().abs() * 0.6;
                Point {
                    x: 4.0 + t,
                    y: 20.0 + (t / 3.0).sin() * 8.0 + (t * 2.0).sin(),
                    pressure,
                }
            })
            .collect();
        let stroke = Stroke::new(StrokeStyle::PEN, Rgba8::BLACK, 3.0, pts);
        let points = stroke.points();
        let mut finished = render(stroke.clone());
        let mut wet = cairo::ImageSurface::create(cairo::Format::ARgb32, 40, 40).unwrap();
        {
            let cr = cairo::Context::new(&wet).unwrap();
            for k in 0..points.len() {
                let new = &points[k.saturating_sub(1)..=k];
                let r = new
                    .iter()
                    .map(|p| pen_radius(3.0, p.pressure))
                    .fold(0.0, f64::max);
                let b = new
                    .iter()
                    .map(|p| disc_box(*p, r + 1.0))
                    .reduce(Rect::union)
                    .unwrap();
                let (x0, y0) = (b.x0.floor(), b.y0.floor());
                let (x1, y1) = (b.x1.ceil(), b.y1.ceil());
                let area = Rect { x0, y0, x1, y1 };
                cr.save().unwrap();
                cr.rectangle(x0.into(), y0.into(), (x1 - x0).into(), (y1 - y0).into());
                cr.clip();
                cr.set_operator(cairo::Operator::Clear);
                cr.paint().unwrap();
                cr.set_operator(cairo::Operator::Over);
                cr.new_path();
                add_pen_path(&cr, 3.0, &points[..=k], Some(area));
                cr.fill().unwrap();
                cr.restore().unwrap();
            }
        }
        let worst = (0..40)
            .flat_map(|y| (0..40).map(move |x| (x, y)))
            .map(|(x, y)| alpha_at(&mut finished, x, y).abs_diff(alpha_at(&mut wet, x, y)))
            .max()
            .unwrap();
        assert!(worst <= 2, "alpha differs by up to {worst}");
    }

    #[test]
    fn eraser_hits_near_the_ink_only() {
        let pts = [(10.0, 10.0), (50.0, 10.0)].map(|(x, y)| Point {
            x,
            y,
            pressure: 1.0,
        });
        let s = Stroke::new(StrokeStyle::PEN, Rgba8::BLACK, 2.0, pts);
        assert!(hits(&s, (30.0, 12.5), 2.0), "within radius + half width");
        assert!(!hits(&s, (30.0, 14.0), 2.0));
        assert!(hits(&s, (8.0, 10.0), 1.5), "round end");
        let dot = Stroke::new(StrokeStyle::HIGHLIGHTER, Rgba8::BLACK, 10.0, [pts[0]]);
        assert!(hits(&dot, (16.0, 10.0), 2.0));
    }

    #[test]
    fn a_dragged_path_touches_what_it_crosses() {
        let pts = [(10.0, 10.0), (50.0, 10.0)].map(|(x, y)| Point {
            x,
            y,
            pressure: 1.0,
        });
        let s = Stroke::new(StrokeStyle::PEN, Rgba8::BLACK, 1.0, pts);
        // Samples far apart on either side of the line: only the segment between them crosses.
        assert!(touches_path(&s, &[(30.0, 0.0), (30.0, 20.0)], 0.5));
        assert!(!touches_path(&s, &[(30.0, 0.0), (30.0, 8.0)], 0.5));
        assert!(
            touches_path(&s, &[(30.0, 0.0), (30.0, 9.0)], 0.5),
            "within reach"
        );
        assert!(
            !touches_path(&s, &[(60.0, 0.0), (60.0, 20.0)], 0.5),
            "past the end"
        );
        assert!(touches_path(&s, &[(20.0, 10.2)], 0.5), "a tap");
        assert!(!touches_path(&s, &[], 0.5));
        let dot = Stroke::new(StrokeStyle::PEN, Rgba8::BLACK, 2.0, [pts[0]]);
        assert!(touches_path(&dot, &[(0.0, 10.0), (20.0, 10.0)], 0.5));
    }

    #[test]
    fn highlighter_is_translucent_and_uniform() {
        let pts = [(5.0, 20.0), (35.0, 20.0), (5.0, 20.5)].map(|(x, y)| Point {
            x,
            y,
            pressure: 1.0,
        });
        let mut s = render(Stroke::new(
            StrokeStyle::HIGHLIGHTER,
            Rgba8::new(255, 230, 0, 90),
            6.0,
            pts,
        ));
        let (a, b) = (alpha_at(&mut s, 10, 20), alpha_at(&mut s, 30, 20));
        assert_eq!(
            a, b,
            "one stroke operation, no double alpha where it overlaps itself"
        );
        assert!((85..=95).contains(&a));
    }

    fn style(layer: Layer, roundness: u8) -> StrokeStyle {
        StrokeStyle {
            layer,
            roundness,
            ..StrokeStyle::PEN
        }
    }

    #[test]
    fn square_nib_fills_its_corners() {
        let at = |x, y| Point {
            x,
            y,
            pressure: 1.0,
        };
        let square = Stroke::new(style(Layer::Ink, 0), Rgba8::BLACK, 12.0, [at(20.0, 20.0)]);
        let mut s = render(square.clone());
        // Corner of the 12 pt square: outside the disc a round nib would draw.
        assert_eq!(alpha_at(&mut s, 25, 25), 255);
        assert_eq!(alpha_at(&mut s, 27, 20), 0);
        let mut round = render(Stroke::new(
            StrokeStyle::PEN,
            Rgba8::BLACK,
            12.0,
            [at(20.0, 20.0)],
        ));
        assert_eq!(alpha_at(&mut round, 25, 25), 0);
        assert!(hits(&square, (26.0, 26.0), 0.5), "at the corner");
        assert!(!hits(&square, (27.0, 27.0), 0.5));
    }

    #[test]
    fn nib_strokes_are_one_flat_fill() {
        // A zig-zag whose hulls overlap: translucent, it must not darken where they do.
        let pts = [(5.0, 10.0), (35.0, 20.0), (5.0, 30.0), (35.0, 22.0)].map(|(x, y)| Point {
            x,
            y,
            pressure: 0.7,
        });
        let mut s = render(Stroke::new(
            style(Layer::Ink, 100),
            Rgba8::new(0, 0, 0, 128),
            6.0,
            pts,
        ));
        // (20, 15) is on the first segment only; (33, 20) where it meets the second.
        let (a, b) = (alpha_at(&mut s, 20, 15), alpha_at(&mut s, 33, 20));
        assert_eq!(a, b);
        assert!((120..=136).contains(&a), "{a}");
    }

    #[test]
    fn hulls_wind_positively() {
        let mut corners = Vec::new();
        nib_corners(&mut corners, (0.0, 0.0), 2.0, 0.5);
        nib_corners(&mut corners, (7.0, -3.0), 1.0, 0.5);
        let mut hull = Vec::new();
        convex_hull(&mut corners, &mut hull);
        assert!(signed_area(&hull) > 0.0);
        assert!(hull.len() >= 4 && hull.len() <= corners.len());
    }

    #[test]
    fn layers_draw_under_ink_then_over() {
        let line = |y| {
            [(5.0, y), (35.0, y)].map(|(x, y)| Point {
                x,
                y,
                pressure: 1.0,
            })
        };
        let red = Rgba8::new(255, 0, 0, 255);
        let blue = Rgba8::new(0, 0, 255, 255);
        let pixel = |strokes: &[Arc<Stroke>]| {
            let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 40, 40).unwrap();
            let cr = cairo::Context::new(&surface).unwrap();
            draw_ink(&cr, strokes, None);
            drop(cr);
            let stride = surface.stride() as usize;
            let data = surface.data().unwrap();
            data[20 * stride + 20 * 4] // blue channel (BGRA)
        };
        // Over drawn first in page order, ink after: over still ends on top.
        let over = Arc::new(Stroke::new(style(Layer::Over, 255), blue, 8.0, line(20.0)));
        let ink = Arc::new(Stroke::new(style(Layer::Ink, 255), red, 8.0, line(20.0)));
        assert_eq!(pixel(&[over.clone(), ink.clone()]), 255);
        let under = Arc::new(Stroke::new(style(Layer::Under, 255), blue, 8.0, line(20.0)));
        assert_eq!(pixel(&[ink, under]), 0);
    }
}
