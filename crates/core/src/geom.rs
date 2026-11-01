/// Axis-aligned rectangle in page points; `x0 <= x1` and `y0 <= y1`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Rect {
    /// The smallest rectangle containing all `points` (x, y).
    pub(crate) fn around(points: impl IntoIterator<Item = (f32, f32)>) -> Self {
        let mut r = Rect {
            x0: f32::INFINITY,
            y0: f32::INFINITY,
            x1: f32::NEG_INFINITY,
            y1: f32::NEG_INFINITY,
        };
        for (x, y) in points {
            r.x0 = r.x0.min(x);
            r.y0 = r.y0.min(y);
            r.x1 = r.x1.max(x);
            r.y1 = r.y1.max(y);
        }
        r
    }

    pub fn expanded(self, by: f32) -> Self {
        Rect {
            x0: self.x0 - by,
            y0: self.y0 - by,
            x1: self.x1 + by,
            y1: self.y1 + by,
        }
    }

    pub fn union(self, o: Rect) -> Self {
        Rect {
            x0: self.x0.min(o.x0),
            y0: self.y0.min(o.y0),
            x1: self.x1.max(o.x1),
            y1: self.y1.max(o.y1),
        }
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.x0 <= other.x1 && other.x0 <= self.x1 && self.y0 <= other.y1 && other.y0 <= self.y1
    }

    pub fn contains(&self, (x, y): (f32, f32)) -> bool {
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }

    pub fn translated(self, dx: f32, dy: f32) -> Self {
        Rect {
            x0: self.x0 + dx,
            y0: self.y0 + dy,
            x1: self.x1 + dx,
            y1: self.y1 + dy,
        }
    }
}

/// Whether `p` is inside the closed `polygon` (even-odd rule), as for a lasso.
pub fn polygon_contains(polygon: &[(f32, f32)], (x, y): (f32, f32)) -> bool {
    let mut inside = false;
    let mut prev = match polygon.last() {
        Some(&p) => p,
        None => return false,
    };
    for &(px, py) in polygon {
        let (qx, qy) = prev;
        if (py > y) != (qy > y) && x < (qx - px) * (y - py) / (qy - py) + px {
            inside = !inside;
        }
        prev = (px, py);
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polygon_contains_handles_concave_shapes() {
        // A "U": the notch at the top middle is outside.
        let u = [
            (0.0, 0.0),
            (3.0, 0.0),
            (3.0, 3.0),
            (2.0, 3.0),
            (2.0, 1.0),
            (1.0, 1.0),
            (1.0, 3.0),
            (0.0, 3.0),
        ];
        assert!(polygon_contains(&u, (0.5, 2.0)));
        assert!(polygon_contains(&u, (1.5, 0.5)));
        assert!(!polygon_contains(&u, (1.5, 2.0)));
        assert!(!polygon_contains(&u, (4.0, 1.0)));
        assert!(!polygon_contains(&[], (0.0, 0.0)));
    }
}
