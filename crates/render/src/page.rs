// SPDX-License-Identifier: GPL-3.0-or-later
//! Whole pages in page points: paper, PDF background and ink, back to front. Used for page
//! thumbnails and for export; the canvas draws paper with GTK nodes and the rest as tiles,
//! with the same geometry.

use std::sync::Arc;

use cera_core::{PageSpec, PaperKind, Rgba8, Stroke};

use crate::ink;

/// Paper rule width and dot size in points, as the canvas draws them.
const RULE_PT: f64 = 0.5;
const DOT_PT: f64 = 1.5;

fn set_rgba(cr: &cairo::Context, c: Rgba8) {
    cr.set_source_rgba(
        f64::from(c.r) / 255.0,
        f64::from(c.g) / 255.0,
        f64::from(c.b) / 255.0,
        f64::from(c.a) / 255.0,
    );
}

/// Fill and pattern. Lines sit at the end of each spacing cell, as on the canvas.
pub fn draw_paper(cr: &cairo::Context, spec: &PageSpec) {
    let (w, h) = (f64::from(spec.width), f64::from(spec.height));
    let paper = spec.paper;
    set_rgba(cr, paper.fill);
    cr.rectangle(0.0, 0.0, w, h);
    let _ = cr.fill();
    let spacing = f64::from(paper.spacing);
    if paper.kind == PaperKind::Blank || spacing < 1.0 {
        return;
    }
    set_rgba(cr, paper.line);
    let steps = |len: f64| {
        (1..)
            .map(move |i| f64::from(i) * spacing)
            .take_while(move |&v| v <= len)
    };
    match paper.kind {
        PaperKind::Ruled => {
            for y in steps(h) {
                cr.rectangle(0.0, y - RULE_PT, w, RULE_PT);
            }
        }
        PaperKind::Grid => {
            for y in steps(h) {
                cr.rectangle(0.0, y - RULE_PT, w, RULE_PT);
            }
            for x in steps(w) {
                cr.rectangle(x - RULE_PT, 0.0, RULE_PT, h);
            }
        }
        PaperKind::Dots => {
            for y in steps(h) {
                for x in steps(w) {
                    cr.rectangle(x - DOT_PT, y - DOT_PT, DOT_PT, DOT_PT);
                }
            }
        }
        PaperKind::Blank => {}
    }
    let _ = cr.fill();
}

/// The whole page. `pdf` is the background page, if any; `printing` renders it the way
/// Poppler prints (annotations as printed, for export).
pub fn draw_page(
    cr: &cairo::Context,
    spec: &PageSpec,
    pdf: Option<&poppler::Page>,
    strokes: &[Arc<Stroke>],
    printing: bool,
) {
    draw_paper(cr, spec);
    if let Some(page) = pdf {
        cr.save().ok();
        if printing {
            page.render_for_printing(cr);
        } else {
            page.render(cr);
        }
        cr.restore().ok();
    }
    ink::draw_ink(cr, strokes, None);
}
