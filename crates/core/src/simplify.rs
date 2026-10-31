//! Ramer–Douglas–Peucker simplification of stylus samples before a stroke is stored.

use crate::model::Point;

/// Drops points that lie within `eps` points of the line between their kept neighbours and
/// whose pressure is within `pressure_eps` of the value interpolated along it. The first and
/// last points are always kept.
pub fn simplify(points: &[Point], eps: f32, pressure_eps: f32) -> Vec<Point> {
    if points.len() <= 2 {
        return points.to_vec();
    }
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;
    // Iterative, so a long stroke can't overflow the stack.
    let mut spans = vec![(0, points.len() - 1)];
    while let Some((a, b)) = spans.pop() {
        let mut worst = (0.0f32, 0);
        for i in a + 1..b {
            let excess = deviation(points[a], points[b], points[i], eps, pressure_eps);
            if excess > worst.0 {
                worst = (excess, i);
            }
        }
        // `deviation` is above 1 when a tolerance is exceeded.
        if worst.0 > 1.0 {
            keep[worst.1] = true;
            spans.push((a, worst.1));
            spans.push((worst.1, b));
        }
    }
    points
        .iter()
        .zip(keep)
        .filter_map(|(p, k)| k.then_some(*p))
        .collect()
}

/// How far `p` is from segment `a`–`b`, as a multiple of the tolerances (max of position and
/// pressure).
fn deviation(a: Point, b: Point, p: Point, eps: f32, pressure_eps: f32) -> f32 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 {
        (((p.x - a.x) * dx + (p.y - a.y) * dy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (cx, cy) = (a.x + t * dx, a.y + t * dy);
    let dist = ((p.x - cx).powi(2) + (p.y - cy).powi(2)).sqrt();
    let pressure = a.pressure + t * (b.pressure - a.pressure);
    (dist / eps).max((p.pressure - pressure).abs() / pressure_eps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: f32, y: f32, pressure: f32) -> Point {
        Point { x, y, pressure }
    }

    #[test]
    fn collinear_points_collapse_to_the_ends() {
        let line: Vec<_> = (0..50).map(|i| pt(i as f32, 2.0 * i as f32, 0.5)).collect();
        assert_eq!(simplify(&line, 0.05, 0.05), [line[0], line[49]]);
    }

    #[test]
    fn corners_and_pressure_changes_are_kept() {
        let mut v: Vec<_> = (0..=10).map(|i| pt(i as f32, 0.0, 0.5)).collect();
        v.extend((1..=10).map(|i| pt(10.0, i as f32, 0.5)));
        let s = simplify(&v, 0.05, 0.05);
        assert_eq!(s, [v[0], v[10], v[20]]);

        let mut p: Vec<_> = (0..=10).map(|i| pt(i as f32, 0.0, 0.2)).collect();
        p[5].pressure = 0.9;
        assert!(simplify(&p, 0.05, 0.05).contains(&p[5]));
    }

    #[test]
    fn short_inputs_are_untouched() {
        assert!(simplify(&[], 0.05, 0.05).is_empty());
        let one = [pt(1.0, 1.0, 0.3)];
        assert_eq!(simplify(&one, 0.05, 0.05), one);
    }
}
