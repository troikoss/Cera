//! Stroke geometry and drawing. The render workers (finished ink tiles) and the canvas (wet
//! ink while writing) both draw with these functions, so the stroke doesn't change when the
//! finished tile replaces the wet one.
//!
//! A pen stroke is the union of a disc at every point, its radius from the pressure, and the
//! hull between neighbouring discs, filled once (no overlapping alpha, round ends for free).
//! Finished strokes skip the discs where the stroke barely turns: there the hulls already meet
//! to within a thousandth of a pixel, and filling hundreds of discs is what costs time.
//! A highlighter is a constant-width polyline with round joins and caps.

use std::f64::consts::TAU;
use std::sync::Arc;

use cera_core::{Point, Rect, Rgba8, Stroke, Tool};

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

/// Draws a finished stroke in page points; with `area`, only the parts that paint inside it.
pub fn draw_stroke(cr: &cairo::Context, stroke: &Stroke, area: Option<Rect>) {
    set_color(cr, stroke.color(), false);
    let points = stroke.points();
    match stroke.tool() {
        Tool::Pen => {
            cr.new_path();
            cr.set_fill_rule(cairo::FillRule::Winding);
            add_pen_path(cr, stroke.width(), points, area);
            let _ = cr.fill();
        }
        Tool::Highlighter => {
            set_highlighter_line(cr, stroke.width());
            cr.new_path();
            add_highlighter_path(cr, stroke.width(), points, area);
            let _ = cr.stroke();
        }
    }
}

/// Draws strokes in page order, highlighters first so pen ink always sits on top. With
/// `area` (a tile), each stroke adds only its shapes that reach into it.
pub fn draw_ink(cr: &cairo::Context, strokes: &[Arc<Stroke>], area: Option<Rect>) {
    for tool in [Tool::Highlighter, Tool::Pen] {
        for stroke in strokes.iter().filter(|s| s.tool() == tool) {
            draw_stroke(cr, stroke, area);
        }
    }
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
    let reach = |q: &Point| {
        radius
            + match stroke.tool() {
                Tool::Pen => pen_radius(stroke.width(), q.pressure) as f32,
                Tool::Highlighter => stroke.width() / 2.0,
            }
    };
    let points = stroke.points();
    if let [only] = points {
        return distance_to_segment(p, *only, *only) <= reach(only);
    }
    points
        .windows(2)
        .any(|w| distance_to_segment(p, w[0], w[1]) <= reach(&w[0]).max(reach(&w[1])))
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
        let mut s = render(Stroke::new(Tool::Pen, Rgba8::BLACK, 4.0, pts));
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
        let stroke = Stroke::new(Tool::Pen, Rgba8::BLACK, 3.0, pts);
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
        let s = Stroke::new(Tool::Pen, Rgba8::BLACK, 2.0, pts);
        assert!(hits(&s, (30.0, 12.5), 2.0), "within radius + half width");
        assert!(!hits(&s, (30.0, 14.0), 2.0));
        assert!(hits(&s, (8.0, 10.0), 1.5), "round end");
        let dot = Stroke::new(Tool::Highlighter, Rgba8::BLACK, 10.0, [pts[0]]);
        assert!(hits(&dot, (16.0, 10.0), 2.0));
    }

    #[test]
    fn highlighter_is_translucent_and_uniform() {
        let pts = [(5.0, 20.0), (35.0, 20.0), (5.0, 20.5)].map(|(x, y)| Point {
            x,
            y,
            pressure: 0.5,
        });
        let mut s = render(Stroke::new(
            Tool::Highlighter,
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
}
