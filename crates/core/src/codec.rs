// SPDX-License-Identifier: GPL-3.0-or-later
//! `PageInk` chunk payload: a page's strokes as varints.
//!
//! ```text
//! payload = encoding:u8  page_id:varint  count:varint  stroke{count}
//! stroke  = r g b a:u8  layer roundness pattern spacing:u8  width:varint  n:varint  point{n}
//! point   = zigzag(dx):varint  zigzag(dy):varint  pressure:u8
//! ```
//!
//! Coordinates and widths are in 1/64 pt; the first point's delta is from (0, 0), so it is
//! absolute. Pressure is in 1/255 steps. Typically 3–5 bytes per point.
//!
//! Encoding 1 (before 2026.11) had `tool:u8` (0 pen, 1 highlighter) in place of the colour's
//! following style bytes, before the colour. A pen reads as a round ink stroke; a highlighter
//! as a round stroke under the ink at full pressure, since highlighters ignored pressure.

use std::sync::Arc;

use crate::error::{Error, Result, corrupt};
use crate::model::{Layer, PageId, Pattern, Point, Rgba8, Stroke, StrokeStyle};

const ENCODING: u8 = 2;
const UNITS_PER_PT: f32 = 64.0;
/// Clamp so every coordinate is exactly representable as f32 (|units| < 2^24), which makes
/// rounding idempotent. That is ±262 143 pt, far beyond any page.
const MAX_UNITS: i32 = (1 << 24) - 1;
/// Smallest possible encoded stroke: colour, style, width, count, one point (encoding 1:
/// tool instead of the style).
const MIN_STROKE_LEN: usize = 4 + 4 + 1 + 1 + 3;
const MIN_STROKE_LEN_V1: usize = 1 + 4 + 1 + 1 + 3;
const MIN_POINT_LEN: usize = 3;

fn to_units(v: f32) -> i32 {
    // `as` saturates and maps NaN to 0.
    ((v * UNITS_PER_PT).round() as i32).clamp(-MAX_UNITS, MAX_UNITS)
}

fn from_units(u: i32) -> f32 {
    u as f32 / UNITS_PER_PT
}

fn to_pressure(p: f32) -> u8 {
    (p.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn from_pressure(p: u8) -> f32 {
    f32::from(p) / 255.0
}

pub(crate) fn round_point(p: Point) -> Point {
    Point {
        x: from_units(to_units(p.x)),
        y: from_units(to_units(p.y)),
        pressure: from_pressure(to_pressure(p.pressure)),
    }
}

pub(crate) fn round_width(width: f32) -> f32 {
    from_units(to_units(width).max(0))
}

pub(crate) fn encode_page_ink(page: PageId, strokes: &[Arc<Stroke>], out: &mut Vec<u8>) {
    out.push(ENCODING);
    put_varint(out, page.into());
    put_varint(out, strokes.len() as u64);
    for stroke in strokes {
        let c = stroke.color();
        out.extend_from_slice(&[c.r, c.g, c.b, c.a]);
        let style = stroke.style();
        out.extend_from_slice(&[
            match style.layer {
                Layer::Under => 0,
                Layer::Ink => 1,
                Layer::Over => 2,
            },
            style.roundness,
            match style.pattern {
                Pattern::Solid => 0,
                Pattern::Dash => 1,
                Pattern::Dot => 2,
            },
            style.spacing,
        ]);
        put_varint(out, to_units(stroke.width()) as u64);
        put_varint(out, stroke.points().len() as u64);
        let (mut px, mut py) = (0i64, 0i64);
        for p in stroke.points() {
            let (x, y) = (i64::from(to_units(p.x)), i64::from(to_units(p.y)));
            put_varint(out, zigzag(x - px));
            put_varint(out, zigzag(y - py));
            out.push(to_pressure(p.pressure));
            (px, py) = (x, y);
        }
    }
}

/// Decodes a payload written by [`encode_page_ink`], or by encoding 1. Never panics on bad
/// input, and never allocates more than the payload can describe.
pub(crate) fn decode_page_ink(payload: &[u8]) -> Result<(PageId, Vec<Arc<Stroke>>)> {
    let mut r = Reader(payload);
    let encoding = r.u8()?;
    if !(1..=ENCODING).contains(&encoding) {
        return Err(Error::UnsupportedVersion {
            what: "ink encoding",
            found: encoding.into(),
            supported: ENCODING.into(),
        });
    }
    let v1 = encoding == 1;
    let page = PageId::try_from(r.varint()?).map_err(|_| corrupt("page id"))?;
    let min_len = if v1 {
        MIN_STROKE_LEN_V1
    } else {
        MIN_STROKE_LEN
    };
    let count = r.count(min_len, "stroke count")?;
    let mut strokes = Vec::with_capacity(count);
    for _ in 0..count {
        let (style, color) = if v1 {
            let style = match r.u8()? {
                0 => StrokeStyle::PEN,
                1 => StrokeStyle::HIGHLIGHTER,
                _ => return Err(corrupt("unknown tool")),
            };
            let [cr, cg, cb, ca] = r.array()?;
            (style, Rgba8::new(cr, cg, cb, ca))
        } else {
            let [cr, cg, cb, ca] = r.array()?;
            let [layer, roundness, pattern, spacing] = r.array()?;
            let layer = match layer {
                0 => Layer::Under,
                1 => Layer::Ink,
                2 => Layer::Over,
                _ => return Err(corrupt("unknown layer")),
            };
            let pattern = match pattern {
                0 => Pattern::Solid,
                1 => Pattern::Dash,
                2 => Pattern::Dot,
                _ => return Err(corrupt("unknown pattern")),
            };
            let style = StrokeStyle {
                layer,
                roundness,
                pattern,
                spacing,
            };
            (style, Rgba8::new(cr, cg, cb, ca))
        };
        // Encoding 1 highlighters were drawn at one width whatever the pressure.
        let full_pressure = v1 && style == StrokeStyle::HIGHLIGHTER;
        let width = i32::try_from(r.varint()?)
            .ok()
            .filter(|w| *w <= MAX_UNITS)
            .ok_or_else(|| corrupt("width out of range"))?;
        let n = r.count(MIN_POINT_LEN, "point count")?;
        if n == 0 {
            return Err(corrupt("empty stroke"));
        }
        let mut points = Vec::with_capacity(n);
        let (mut x, mut y) = (0i32, 0i32);
        for _ in 0..n {
            x = r.coordinate(x)?;
            y = r.coordinate(y)?;
            let pressure = from_pressure(r.u8()?);
            points.push(Point {
                x: from_units(x),
                y: from_units(y),
                pressure: if full_pressure { 1.0 } else { pressure },
            });
        }
        strokes.push(Arc::new(Stroke::from_rounded(
            style,
            color,
            from_units(width),
            points.into(),
        )));
    }
    if !r.0.is_empty() {
        return Err(corrupt("trailing bytes after strokes"));
    }
    Ok((page, strokes))
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn u8(&mut self) -> Result<u8> {
        let (&b, rest) = self
            .0
            .split_first()
            .ok_or_else(|| corrupt("ink truncated"))?;
        self.0 = rest;
        Ok(b)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let (head, rest) = self
            .0
            .split_first_chunk()
            .ok_or_else(|| corrupt("ink truncated"))?;
        self.0 = rest;
        Ok(*head)
    }

    fn varint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for i in 0..10 {
            let b = self.u8()?;
            if i == 9 && b > 1 {
                return Err(corrupt("varint overflow"));
            }
            v |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(corrupt("varint overflow"))
    }

    /// A count of items that each take at least `min_len` bytes, checked against what's left.
    fn count(&mut self, min_len: usize, what: &str) -> Result<usize> {
        let n = self.varint()?;
        if n > (self.0.len() / min_len) as u64 {
            return Err(corrupt(what));
        }
        Ok(n as usize)
    }

    /// `base` plus a zig-zag delta, within the coordinate range.
    fn coordinate(&mut self, base: i32) -> Result<i32> {
        let d = unzigzag(self.varint()?);
        i64::from(base)
            .checked_add(d)
            .filter(|v| v.unsigned_abs() <= MAX_UNITS as u64)
            .map(|v| v as i32)
            .ok_or_else(|| corrupt("coordinate out of range"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn decode_ok(bytes: &[u8]) -> (PageId, Vec<Arc<Stroke>>) {
        decode_page_ink(bytes).expect("decodes")
    }

    #[test]
    fn varint_and_zigzag_edges() {
        for v in [0, 1, 127, 128, 300, u32::MAX.into(), u64::MAX] {
            let mut out = Vec::new();
            put_varint(&mut out, v);
            assert_eq!(Reader(&out).varint().unwrap(), v);
        }
        for v in [0, 1, -1, 63, -64, i64::MAX, i64::MIN] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
        // 11-byte and overflowing 10-byte varints are rejected.
        assert!(Reader(&[0xff; 11]).varint().is_err());
        assert!(
            Reader(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02])
                .varint()
                .is_err()
        );
    }

    #[test]
    fn handwriting_costs_a_few_bytes_per_point() {
        // A gently curving line sampled every ~0.5 pt, like a stylus at normal speed.
        let points = (0..200).map(|i| {
            let t = i as f32 * 0.5;
            Point {
                x: 100.0 + t,
                y: 300.0 + (t / 8.0).sin() * 6.0,
                pressure: 0.4 + (t / 20.0).sin() * 0.2,
            }
        });
        let stroke = Arc::new(Stroke::new(StrokeStyle::PEN, Rgba8::BLACK, 1.5, points));
        let mut out = Vec::new();
        encode_page_ink(7, &[stroke], &mut out);
        let per_point = out.len() as f32 / 200.0;
        assert!(
            (3.0..=5.0).contains(&per_point),
            "{per_point} bytes per point"
        );
    }

    #[test]
    fn rounding_is_idempotent_and_handles_nan() {
        let p = round_point(Point {
            x: f32::NAN,
            y: 1e30,
            pressure: f32::NAN,
        });
        assert_eq!(
            p,
            Point {
                x: 0.0,
                y: from_units(MAX_UNITS),
                pressure: 0.0
            }
        );
        assert_eq!(round_point(p), p);
    }

    #[test]
    fn rejects_unknown_encoding() {
        for encoding in [0, 3] {
            assert!(matches!(
                decode_page_ink(&[encoding, 0, 0]),
                Err(Error::UnsupportedVersion { .. })
            ));
        }
    }

    fn two_points() -> [Point; 2] {
        [(1.0, 2.0, 0.2), (3.5, 2.0, 0.6)].map(|(x, y, pressure)| Point { x, y, pressure })
    }

    /// Pins encoding 2: a square, dashed highlighter over the ink.
    #[test]
    fn golden_v2() {
        let style = StrokeStyle {
            layer: Layer::Over,
            roundness: 0,
            pattern: Pattern::Dash,
            spacing: 32,
        };
        let stroke = Stroke::new(style, Rgba8::new(10, 20, 30, 90), 2.0, two_points());
        let mut out = Vec::new();
        encode_page_ink(5, &[Arc::new(stroke.clone())], &mut out);
        #[rustfmt::skip]
        let golden = [
            2, 5, 1,                // encoding, page, one stroke
            10, 20, 30, 90,         // colour
            2, 0, 1, 32,            // over, square, dash, spacing 2 widths
            128, 1, 2,              // width 2 pt (128 units), two points
            128, 1, 128, 2, 51,     // (64, 128) units, pressure 0.2
            192, 2, 0, 153,         // dx +160 units (zig-zag 320), dy 0, pressure 0.6
        ];
        assert_eq!(out, golden);
        assert_eq!(decode_ok(&golden).1, [Arc::new(stroke)]);
    }

    /// Encoding 1 files keep reading: a pen, and a highlighter that ignored pressure.
    #[test]
    fn golden_v1() {
        #[rustfmt::skip]
        let v1 = [
            1, 5, 2,                                    // encoding, page, two strokes
            0, 30, 30, 30, 255, 128, 1, 2,              // pen, colour, width 2, two points
            128, 1, 128, 2, 51, 192, 2, 0, 153,
            1, 246, 211, 45, 90, 128, 6, 2,             // highlighter, width 12, two points
            128, 1, 128, 2, 51, 192, 2, 0, 153,
        ];
        let (page, strokes) = decode_ok(&v1);
        assert_eq!(page, 5);
        let pen = Stroke::new(
            StrokeStyle::PEN,
            Rgba8::new(30, 30, 30, 255),
            2.0,
            two_points(),
        );
        let full = two_points().map(|p| Point { pressure: 1.0, ..p });
        let highlighter = Stroke::new(
            StrokeStyle::HIGHLIGHTER,
            Rgba8::new(246, 211, 45, 90),
            12.0,
            full,
        );
        assert_eq!(strokes, [Arc::new(pen), Arc::new(highlighter)]);
        assert!(decode_page_ink(&[1, 5, 1, 2, 0, 0, 0, 0, 0, 1, 0, 0, 0]).is_err());
    }

    fn point() -> impl Strategy<Value = Point> {
        (-2000.0f32..2000.0, -2000.0f32..2000.0, 0.0f32..=1.0).prop_map(|(x, y, pressure)| Point {
            x,
            y,
            pressure,
        })
    }

    fn stroke() -> impl Strategy<Value = Arc<Stroke>> {
        let layer = prop::sample::select(Layer::ALL.to_vec());
        let pattern = prop::sample::select(vec![Pattern::Solid, Pattern::Dash, Pattern::Dot]);
        let style = (layer, any::<u8>(), pattern, any::<u8>()).prop_map(
            |(layer, roundness, pattern, spacing)| StrokeStyle {
                layer,
                roundness,
                pattern,
                spacing,
            },
        );
        (
            style,
            any::<[u8; 4]>(),
            0.0f32..40.0,
            prop::collection::vec(point(), 1..80),
        )
            .prop_map(|(style, [r, g, b, a], width, points)| {
                Arc::new(Stroke::new(style, Rgba8::new(r, g, b, a), width, points))
            })
    }

    proptest! {
        #[test]
        fn round_trip(page in any::<u32>(), strokes in prop::collection::vec(stroke(), 0..20)) {
            let mut out = Vec::new();
            encode_page_ink(page, &strokes, &mut out);
            let (decoded_page, decoded) = decode_ok(&out);
            prop_assert_eq!(decoded_page, page);
            prop_assert_eq!(decoded, strokes);
        }

        #[test]
        fn garbage_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
            let _ = decode_page_ink(&bytes);
        }

        #[test]
        fn every_truncation_is_an_error(strokes in prop::collection::vec(stroke(), 1..4)) {
            let mut out = Vec::new();
            encode_page_ink(1, &strokes, &mut out);
            for len in 0..out.len() {
                prop_assert!(decode_page_ink(&out[..len]).is_err());
            }
        }
    }
}
