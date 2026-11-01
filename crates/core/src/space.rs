// SPDX-License-Identifier: GPL-3.0-or-later
//! Add space that spills over (the lasso's add-space mode, DESIGN.md §9).
//!
//! Add space takes the ink on one side of a line, below or above it, and moves it away from
//! the line (adding space) or back towards it (removing space). The pages are treated as one
//! strip: a stroke pushed past a page's edge lands on the neighbouring page exactly where it
//! was dropped (the pages sit `page_gap` points apart), or further on if it was dragged past
//! that page too. A stroke straddling an edge goes to the page holding most of it and is
//! nudged just enough to fit. A page that receives ink moves its own ink away only as far as
//! needed to keep clear of what arrived, so it may spill in turn; pages are added at the end
//! (or the start) if the last (or first) one overflows.

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::model::{Document, PageSpec, Stroke};
use crate::undo::{Changes, History};

/// Gap (points) kept between ink that spilled onto a page and that page's own ink.
pub const SPILL_GAP: f32 = 18.0;

/// Which side of the line add space took ink from, and so which way it spills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// Ink below the line; moving it down adds space and spills onto the following pages.
    Below,
    /// Ink above the line; moving it up adds space and spills onto the preceding pages.
    Above,
}

impl History {
    /// Moves strokes `indices` of page `index`, taken from `side` of a line, by `dy` points
    /// (down if positive). Moved away from the line, strokes that then lie mostly past the
    /// page's edge spill onto the neighbouring pages, as the module explains; a page added
    /// at either end gets `new_page(the spec of the page beside it)`. One undo step. Returns
    /// what changed, for redrawing.
    #[allow(clippy::too_many_arguments)]
    pub fn move_space(
        &mut self,
        doc: &mut Document,
        index: usize,
        indices: &[usize],
        side: Side,
        dy: f32,
        page_gap: f32,
        new_page: impl Fn(&PageSpec) -> PageSpec,
    ) -> Result<Changes> {
        self.begin_group();
        let result = self.cascade(doc, index, indices, side, dy, page_gap, new_page);
        self.end_group();
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn cascade(
        &mut self,
        doc: &mut Document,
        mut index: usize,
        indices: &[usize],
        side: Side,
        mut dy: f32,
        page_gap: f32,
        new_page: impl Fn(&PageSpec) -> PageSpec,
    ) -> Result<Changes> {
        let below = side == Side::Below;
        let away = if below { dy > 0.0 } else { dy < 0.0 };
        let mut changes = Changes::default();
        let mut moving = indices.to_vec();
        // Strokes from the neighbouring page, already in this page's coordinates.
        let mut arrivals: Vec<Arc<Stroke>> = Vec::new();
        loop {
            let page = doc.page(index)?;
            let (id, height) = (page.id(), page.spec().height);
            let strokes = doc.load_ink(index)?.to_vec();
            let mut kept = Vec::new();
            let (mut onward, mut onward_at) = (Vec::new(), Vec::new());
            for &i in &moving {
                let old = strokes.get(i).ok_or(Error::StrokeOutOfRange(i))?;
                let new = Arc::new(old.translated(0.0, dy));
                let b = new.bounds();
                changes.add_ink(id, old.bounds().union(b));
                let middle = (b.y0 + b.y1) / 2.0;
                let past = if below { middle > height } else { middle < 0.0 };
                if away && past {
                    onward_at.push(i);
                    onward.push(new);
                } else if away {
                    kept.push((i, fit(new, height, side)));
                } else {
                    kept.push((i, new));
                }
            }
            // An arrival moves on only if it lies wholly past the edge: one taller than a
            // page then stays here instead of travelling forever.
            let mut staying = Vec::new();
            for a in std::mem::take(&mut arrivals) {
                let b = a.bounds();
                if (below && b.y0 >= height) || (!below && b.y1 <= 0.0) {
                    onward.push(a);
                } else {
                    let a = fit(a, height, side);
                    changes.add_ink(id, a.bounds());
                    staying.push(a);
                }
            }
            // Replacing keeps indices, so the removal after it still addresses the right ones.
            self.replace_strokes(doc, id, kept)?;
            self.remove_strokes(doc, id, &onward_at)?;
            self.add_strokes(doc, id, staying)?;

            if onward.is_empty() {
                return Ok(changes);
            }
            // The neighbouring page, added if there is none.
            let added = if below {
                index += 1;
                index == doc.page_count()
            } else if index == 0 {
                true
            } else {
                index -= 1;
                false
            };
            if added {
                let beside = if below { index - 1 } else { 0 };
                let spec = new_page(doc.page(beside)?.spec());
                self.insert_page(doc, index, spec)?;
                changes.layout = true;
            }
            let next_height = doc.page(index)?.spec().height;
            // Into its coordinates, nudged onto it if they straddled the gap.
            let shift = if below {
                -(height + page_gap)
            } else {
                next_height + page_gap
            };
            arrivals = onward
                .iter()
                .map(|s| {
                    let s = s.translated(0.0, shift);
                    let b = s.bounds();
                    let nudge = if below {
                        (-b.y0).max(0.0)
                    } else {
                        -(b.y1 - next_height).max(0.0)
                    };
                    Arc::new(if nudge != 0.0 {
                        s.translated(0.0, nudge)
                    } else {
                        s
                    })
                })
                .collect();
            if added {
                moving.clear();
                dy = 0.0;
                continue;
            }
            let own = doc.load_ink(index)?;
            // Its own ink keeps clear of the arrivals that stay on it.
            let staying = arrivals.iter().map(|s| s.bounds());
            dy = if below {
                let bottom = staying
                    .filter(|b| b.y0 < next_height)
                    .map(|b| b.y1.min(next_height))
                    .reduce(f32::max);
                match (bottom, top(own)) {
                    (Some(bottom), Some(top)) => (bottom + SPILL_GAP - top).max(0.0),
                    _ => 0.0,
                }
            } else {
                let top = staying
                    .filter(|b| b.y1 > 0.0)
                    .map(|b| b.y0.max(0.0))
                    .reduce(f32::min);
                match (top, bottom(own)) {
                    (Some(top), Some(bottom)) => (top - SPILL_GAP - bottom).min(0.0),
                    _ => 0.0,
                }
            };
            moving = if dy != 0.0 {
                (0..own.len()).collect()
            } else {
                Vec::new()
            };
        }
    }
}

/// Moves a stroke that hangs over the page's edge on `side` back just enough to fit, but
/// not past the other edge. One taller than the page can't fit and stays where it is.
fn fit(s: Arc<Stroke>, height: f32, side: Side) -> Arc<Stroke> {
    let b = s.bounds();
    if b.y1 - b.y0 > height {
        return s;
    }
    let dy = match side {
        Side::Below => -(b.y1 - height).min(b.y0).max(0.0),
        Side::Above => (-b.y0).min(height - b.y1).max(0.0),
    };
    if dy != 0.0 {
        Arc::new(s.translated(0.0, dy))
    } else {
        s
    }
}

/// The bottom of the lowest ink.
fn bottom(strokes: &[Arc<Stroke>]) -> Option<f32> {
    strokes.iter().map(|s| s.bounds().y1).reduce(f32::max)
}

/// The top of the topmost ink.
fn top(strokes: &[Arc<Stroke>]) -> Option<f32> {
    strokes.iter().map(|s| s.bounds().y0).reduce(f32::min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Paper, Point, Rgba8, Tool};

    const GAP: f32 = 10.0;

    /// A short horizontal stroke 1 pt wide at height `y`; its bounds span y ± 0.5.
    fn stroke(y: f32) -> Arc<Stroke> {
        let points = (0..3).map(|i| Point {
            x: 10.0 + i as f32,
            y,
            pressure: 0.5,
        });
        Arc::new(Stroke::new(Tool::Pen, Rgba8::BLACK, 1.0, points))
    }

    fn vertical(y0: f32, y1: f32) -> Arc<Stroke> {
        Arc::new(Stroke::new(
            Tool::Pen,
            Rgba8::BLACK,
            1.0,
            [y0, y1].map(|y| Point {
                x: 10.0,
                y,
                pressure: 0.5,
            }),
        ))
    }

    fn ys(doc: &Document, index: usize) -> Vec<f32> {
        doc.pages()[index]
            .strokes()
            .unwrap()
            .iter()
            .map(|s| s.points()[0].y)
            .collect()
    }

    /// Pages 100 pt tall.
    fn doc(pages: &[&[f32]]) -> (Document, History) {
        let spec = PageSpec::new((100.0, 100.0), Paper::BLANK);
        let mut d = Document::new(vec![spec; pages.len()]);
        let mut h = History::default();
        for (i, ys) in pages.iter().enumerate() {
            h.add_strokes(&mut d, i as u32, ys.iter().map(|&y| stroke(y)).collect())
                .unwrap();
        }
        (d, h)
    }

    fn same(spec: &PageSpec) -> PageSpec {
        *spec
    }

    #[test]
    fn stays_on_the_page_when_there_is_room() {
        let (mut d, mut h) = doc(&[&[10.0, 50.0], &[20.0]]);
        let c = h
            .move_space(&mut d, 0, &[1], Side::Below, 30.0, GAP, same)
            .unwrap();
        assert_eq!(ys(&d, 0), [10.0, 80.0]);
        assert_eq!(ys(&d, 1), [20.0]);
        assert!(!c.layout);
        assert_eq!(c.ink.len(), 1);
    }

    #[test]
    fn lands_where_it_was_dropped() {
        let (mut d, mut h) = doc(&[&[10.0, 50.0, 70.0], &[60.0]]);
        h.move_space(&mut d, 0, &[1, 2], Side::Below, 50.0, GAP, same)
            .unwrap();
        // 50 → 100 hangs half over the edge, so it is nudged up to fit; 70 → 120 is 20 pt
        // past the edge, which is 10 pt onto the next page past the gap.
        assert_eq!(ys(&d, 0), [10.0, 99.5]);
        // Page 1's ink is well below the arrival, so it stays.
        assert_eq!(ys(&d, 1), [60.0, 10.0]);
    }

    #[test]
    fn next_page_ink_makes_room() {
        let (mut d, mut h) = doc(&[&[70.0], &[20.0, 40.0]]);
        h.move_space(&mut d, 0, &[0], Side::Below, 50.0, GAP, same)
            .unwrap();
        // The arrival sits at 10 (bottom 10.5); page 1's ink starts at 19.5 and moves down to
        // stay SPILL_GAP below it.
        let push = 10.5 + SPILL_GAP - 19.5;
        assert_eq!(ys(&d, 1), [20.0 + push, 40.0 + push, 10.0]);
    }

    #[test]
    fn a_long_drag_skips_pages() {
        let (mut d, mut h) = doc(&[&[10.0, 80.0], &[], &[]]);
        h.move_space(&mut d, 0, &[1], Side::Below, 230.0, GAP, same)
            .unwrap();
        // 310 on page 0 is 200 on page 1 and 90 on page 2.
        assert_eq!(ys(&d, 0), [10.0]);
        assert!(ys(&d, 1).is_empty());
        assert_eq!(ys(&d, 2), [90.0]);
    }

    #[test]
    fn straddling_stroke_is_nudged_onto_the_next_page() {
        let (mut d, mut h) = doc(&[&[], &[]]);
        h.add_strokes(&mut d, 0, vec![vertical(80.0, 96.0)])
            .unwrap();
        // Moved to 94..110 it is mostly below the edge; on page 1 it would start 16 pt up in
        // the gap, so it is nudged down until its ink starts at the top.
        h.move_space(&mut d, 0, &[0], Side::Below, 14.0, GAP, same)
            .unwrap();
        assert!(ys(&d, 0).is_empty());
        assert_eq!(ys(&d, 1), [0.5]);
    }

    #[test]
    fn cascades_and_adds_a_page_at_the_end() {
        let (mut d, mut h) = doc(&[&[10.0, 90.0], &[10.0, 95.0]]);
        let before: Vec<_> = (0..2).map(|i| ys(&d, i)).collect();
        let c = h
            .move_space(&mut d, 0, &[1], Side::Below, 20.0, GAP, same)
            .unwrap();
        assert!(c.layout);
        assert_eq!(d.page_count(), 3);
        assert_eq!(ys(&d, 0), [10.0]);
        // The arrival lands at 0.5 (nudged out of the gap); page 1's ink moves down 9.5 to
        // keep the gap, which pushes 95 to 104.5, onto a new page (nudged to its top).
        assert_eq!(ys(&d, 1), [19.5, 0.5]);
        assert_eq!(ys(&d, 2), [0.5]);
        assert_eq!(c.ink.len(), 3);

        // One undo step brings everything back.
        h.undo(&mut d).unwrap().unwrap();
        assert_eq!(d.page_count(), 2);
        assert_eq!((0..2).map(|i| ys(&d, i)).collect::<Vec<_>>(), before);
        h.redo(&mut d).unwrap().unwrap();
        assert_eq!(d.page_count(), 3);
    }

    #[test]
    fn tall_strokes_never_loop() {
        let spec = PageSpec::new((100.0, 100.0), Paper::BLANK);
        let mut d = Document::new(vec![spec; 2]);
        let mut h = History::default();
        h.add_strokes(&mut d, 0, vec![stroke(5.0), vertical(50.0, 250.0)])
            .unwrap();
        h.move_space(&mut d, 0, &[1], Side::Below, 100.0, GAP, same)
            .unwrap();
        // Mostly below the edge, it moves to page 1 and stays, hanging over its edge.
        assert_eq!(d.page_count(), 2);
        assert_eq!(ys(&d, 1), [40.0]);
    }

    #[test]
    fn moving_up_never_spills() {
        let (mut d, mut h) = doc(&[&[10.0, 99.0], &[20.0]]);
        h.move_space(&mut d, 0, &[1], Side::Below, -30.0, GAP, same)
            .unwrap();
        assert_eq!(ys(&d, 0), [10.0, 69.0]);
        assert_eq!(ys(&d, 1), [20.0]);
    }

    #[test]
    fn above_spills_onto_the_previous_page() {
        let (mut d, mut h) = doc(&[&[], &[10.0, 50.0]]);
        h.move_space(&mut d, 1, &[0, 1], Side::Above, -30.0, GAP, same)
            .unwrap();
        // 10 → -20 is mostly above the edge: 90 on page 0, past the gap.
        assert_eq!(ys(&d, 1), [20.0]);
        assert_eq!(ys(&d, 0), [90.0]);
    }

    #[test]
    fn above_pushes_the_previous_page_up() {
        let (mut d, mut h) = doc(&[&[60.0, 85.0], &[10.0]]);
        h.move_space(&mut d, 1, &[0], Side::Above, -20.0, GAP, same)
            .unwrap();
        // The arrival lands at 100, nudged up to 99.5 to fit; page 0's ink (bottom 85.5)
        // moves up to stay SPILL_GAP above its top (99).
        let up = 99.0 - SPILL_GAP - 85.5;
        assert_eq!(ys(&d, 0), [60.0 + up, 85.0 + up, 99.5]);
        assert!(ys(&d, 1).is_empty());
    }

    #[test]
    fn above_adds_a_page_at_the_start() {
        let (mut d, mut h) = doc(&[&[10.0, 50.0]]);
        let c = h
            .move_space(&mut d, 0, &[0], Side::Above, -30.0, GAP, same)
            .unwrap();
        assert!(c.layout);
        assert_eq!(d.page_count(), 2);
        assert_eq!(ys(&d, 0), [90.0]);
        assert_eq!(ys(&d, 1), [50.0]);
        h.undo(&mut d).unwrap().unwrap();
        assert_eq!(d.page_count(), 1);
        assert_eq!(ys(&d, 0), [10.0, 50.0]);
    }

    #[test]
    fn moving_above_ink_down_never_spills() {
        let (mut d, mut h) = doc(&[&[10.0], &[5.0, 40.0]]);
        h.move_space(&mut d, 1, &[0], Side::Above, 20.0, GAP, same)
            .unwrap();
        assert_eq!(ys(&d, 0), [10.0]);
        assert_eq!(ys(&d, 1), [25.0, 40.0]);
    }
}
