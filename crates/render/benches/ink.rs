// SPDX-License-Identifier: GPL-3.0-or-later
//! `cargo bench -p cera-render`: pen geometry, an ink tile with 500 strokes, and the linear
//! stroke scan that stands in for an R-tree (DESIGN.md §5).

use std::hint::black_box;
use std::sync::Arc;

use cera_core::{Point, Rect, Rgba8, Stroke, StrokeStyle, simplify};
use cera_render::ink;
use criterion::{Criterion, criterion_group, criterion_main};

/// Raw samples of a handwriting-like stroke: `n` points ~0.5 pt apart, wavy, with varying
/// pressure.
fn samples(seed: u32, n: usize, origin: (f32, f32)) -> Vec<Point> {
    let phase = seed as f32 * 0.7;
    (0..n)
        .map(|i| {
            let t = i as f32 * 0.5;
            Point {
                x: origin.0 + t,
                y: origin.1 + (t / 4.0 + phase).sin() * 5.0 + (t * 1.3).sin() * 0.8,
                pressure: 0.4 + (t / 10.0 + phase).sin() * 0.3,
            }
        })
        .collect()
}

/// The stroke as stored: simplified like the canvas does on pen-up. One in ten is a
/// highlighter, which has pressure off.
fn wavy(seed: u32, n: usize, origin: (f32, f32)) -> Arc<Stroke> {
    let style = if seed.is_multiple_of(10) {
        StrokeStyle::HIGHLIGHTER
    } else {
        StrokeStyle::PEN
    };
    styled(seed, n, origin, style)
}

fn styled(seed: u32, n: usize, origin: (f32, f32), style: StrokeStyle) -> Arc<Stroke> {
    let mut raw = samples(seed, n, origin);
    if style == StrokeStyle::HIGHLIGHTER {
        raw.iter_mut().for_each(|p| p.pressure = 1.0);
    }
    let points = simplify(&raw, 0.05, 0.03);
    Arc::new(Stroke::new(style, Rgba8::new(20, 30, 90, 255), 1.6, points))
}

fn nib(roundness: u8) -> StrokeStyle {
    StrokeStyle {
        roundness,
        ..StrokeStyle::PEN
    }
}

fn tile_surface() -> (cairo::ImageSurface, cairo::Context) {
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 256, 256).unwrap();
    let cr = cairo::Context::new(&surface).unwrap();
    cr.scale(2.0, 2.0); // 2 device px per pt
    (surface, cr)
}

fn pen_stroke(c: &mut Criterion) {
    let stroke = wavy(1, 200, (10.0, 60.0));
    println!("200 samples simplified to {} points", stroke.points().len());
    let (_surface, cr) = tile_surface();
    c.bench_function("pen stroke, 200 samples, drawn", |b| {
        b.iter(|| ink::draw_stroke(&cr, black_box(&stroke), None))
    });
    for (name, roundness) in [("square", 0), ("rounded", 128)] {
        let stroke = styled(1, 200, (10.0, 60.0), nib(roundness));
        c.bench_function(&format!("{name} nib stroke, 200 samples, drawn"), |b| {
            b.iter(|| ink::draw_stroke(&cr, black_box(&stroke), None))
        });
    }
    // Wet ink: one motion event adds 4 samples at the head of a 200-sample stroke.
    let raw = samples(1, 200, (10.0, 60.0));
    let head = &raw[195..];
    let area = head
        .iter()
        .map(|p| Rect {
            x0: p.x - 2.0,
            y0: p.y - 2.0,
            x1: p.x + 2.0,
            y1: p.y + 2.0,
        })
        .reduce(Rect::union)
        .unwrap();
    c.bench_function("wet ink event, 4 new samples", |b| {
        b.iter(|| {
            cr.new_path();
            ink::add_pen_path(&cr, 1.6, black_box(&raw), Some(area));
            cr.fill().unwrap();
        })
    });
    let pts = stroke.points();
    c.bench_function("pen hulls, 200 points", |b| {
        b.iter(|| {
            pts.windows(2)
                .filter_map(|w| {
                    let (a, z) = (w[0], w[1]);
                    ink::hull(
                        (f64::from(a.x), f64::from(a.y)),
                        ink::pen_radius(1.6, a.pressure),
                        (f64::from(z.x), f64::from(z.y)),
                        ink::pen_radius(1.6, z.pressure),
                    )
                })
                .count()
        })
    });
}

fn ink_tile(c: &mut Criterion) {
    // 500 short strokes inside one 128 × 128 pt tile, one in ten a highlighter.
    let strokes: Vec<_> = (0..500)
        .map(|i| wavy(i, 60, ((i % 4) as f32 * 25.0, (i / 4 % 120) as f32)))
        .collect();
    let (_surface, cr) = tile_surface();
    c.bench_function("ink tile, 500 strokes", |b| {
        b.iter(|| ink::draw_ink(&cr, black_box(&strokes), None))
    });
    let square: Vec<_> = (0..500)
        .map(|i| styled(i, 60, ((i % 4) as f32 * 25.0, (i / 4 % 120) as f32), nib(0)))
        .collect();
    c.bench_function("ink tile, 500 square-nib strokes", |b| {
        b.iter(|| ink::draw_ink(&cr, black_box(&square), None))
    });
}

fn stroke_scan(c: &mut Criterion) {
    // A densely written page: 3000 strokes over 600 × 800 pt.
    let strokes: Vec<_> = (0..3000)
        .map(|i| wavy(i, 40, ((i % 25) as f32 * 22.0, (i / 25) as f32 * 6.5)))
        .collect();
    let tile = Rect {
        x0: 256.0,
        y0: 384.0,
        x1: 320.0,
        y1: 448.0,
    };
    c.bench_function("strokes in a tile, 3000-stroke page", |b| {
        b.iter(|| ink::strokes_in(black_box(&strokes), tile).len())
    });
}

criterion_group!(benches, pen_stroke, ink_tile, stroke_scan);
criterion_main!(benches);
