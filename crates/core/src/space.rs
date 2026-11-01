// SPDX-License-Identifier: GPL-3.0-or-later
//! Add space that spills over (the lasso's add-space mode, DESIGN.md §9).
//!
//! Strokes pushed down past a page's bottom edge move to the top of the next page, keeping
//! their layout. That page's own ink moves down to make room, so it may spill in turn; the
//! cascade runs page by page and adds a page at the end if the last one overflows. Strokes
//! that arrive on a page never spill again, so a stroke taller than a page can't loop.

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::geom::Rect;
use crate::model::{Document, PageSpec, Stroke};
use crate::undo::{Changes, History};

/// Gap (points) left between ink that spilled onto a page and that page's own ink.
pub const SPILL_GAP: f32 = 18.0;

impl History {
    /// Moves strokes `indices` of page `index` down by `dy` points (up if negative). Strokes
    /// that then cross the bottom edge spill onto the next page, as the module explains;
    /// a page added at the end gets `new_page(last page's spec)`. One undo step. Returns
    /// what changed, for redrawing.
    pub fn push_down(
        &mut self,
        doc: &mut Document,
        index: usize,
        indices: &[usize],
        dy: f32,
        new_page: impl FnOnce(&PageSpec) -> PageSpec,
    ) -> Result<Changes> {
        self.begin_group();
        let result = self.cascade(doc, index, indices, dy, new_page);
        self.end_group();
        result
    }

    fn cascade(
        &mut self,
        doc: &mut Document,
        mut index: usize,
        indices: &[usize],
        mut dy: f32,
        new_page: impl FnOnce(&PageSpec) -> PageSpec,
    ) -> Result<Changes> {
        let mut new_page = Some(new_page);
        let mut changes = Changes::default();
        // Where spilled ink starts on a page with no ink of its own: the source page's top
        // margin, as its topmost ink shows it.
        let margin = top(doc.load_ink(index)?).unwrap_or(0.0).max(0.0);
        let mut moving = indices.to_vec();
        let mut arrivals: Vec<Arc<Stroke>> = Vec::new();
        loop {
            let page = doc.page(index)?;
            let (id, height) = (page.id(), page.spec().height);
            let strokes = doc.load_ink(index)?.to_vec();
            let mut kept = Vec::new();
            let (mut spilled, mut spilled_at) = (Vec::new(), Vec::new());
            for &i in &moving {
                let old = strokes.get(i).ok_or(Error::StrokeOutOfRange(i))?;
                let new = Arc::new(old.translated(0.0, dy));
                changes.add_ink(id, old.bounds().union(new.bounds()));
                if dy > 0.0 && new.bounds().y1 > height {
                    spilled_at.push(i);
                    spilled.push(new);
                } else {
                    kept.push((i, new));
                }
            }
            if let Some(area) = bounds(&arrivals) {
                changes.add_ink(id, area);
            }
            // Replacing keeps indices, so the removal after it still addresses the right ones.
            self.replace_strokes(doc, id, kept)?;
            self.remove_strokes(doc, id, &spilled_at)?;
            self.add_strokes(doc, id, std::mem::take(&mut arrivals))?;

            let Some(block) = bounds(&spilled) else {
                return Ok(changes);
            };
            index += 1;
            let (target, push) = if index < doc.page_count() {
                let next = doc.load_ink(index)?;
                moving = (0..next.len()).collect();
                match top(next) {
                    // Where the next page's ink began, which moves down past the block.
                    Some(t) => (t.max(0.0), block.y1 - block.y0 + SPILL_GAP),
                    None => (margin, 0.0),
                }
            } else {
                // The cascade ends on the page added here, so this runs at most once.
                let new_page = new_page.take().ok_or(Error::HistoryMismatch)?;
                let spec = new_page(doc.page(index - 1)?.spec());
                self.insert_page(doc, index, spec)?;
                changes.layout = true;
                moving.clear();
                (margin, 0.0)
            };
            let shift = target - block.y0;
            arrivals = spilled
                .iter()
                .map(|s| Arc::new(s.translated(0.0, shift)))
                .collect();
            dy = push;
            if push == 0.0 {
                moving.clear();
            }
        }
    }
}

fn bounds(strokes: &[Arc<Stroke>]) -> Option<Rect> {
    strokes.iter().map(|s| s.bounds()).reduce(Rect::union)
}

/// The top of the topmost ink.
fn top(strokes: &[Arc<Stroke>]) -> Option<f32> {
    strokes.iter().map(|s| s.bounds().y0).reduce(f32::min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Paper, Point, Rgba8, Tool};

    /// A short horizontal stroke 1 pt wide at height `y`; its bounds span y ± 0.5.
    fn stroke(y: f32) -> Arc<Stroke> {
        let points = (0..3).map(|i| Point {
            x: 10.0 + i as f32,
            y,
            pressure: 0.5,
        });
        Arc::new(Stroke::new(Tool::Pen, Rgba8::BLACK, 1.0, points))
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
        let c = h.push_down(&mut d, 0, &[1], 30.0, same).unwrap();
        assert_eq!(ys(&d, 0), [10.0, 80.0]);
        assert_eq!(ys(&d, 1), [20.0]);
        assert!(!c.layout);
        assert_eq!(c.ink.len(), 1);
    }

    #[test]
    fn spills_onto_the_next_page_and_pushes_its_ink() {
        let (mut d, mut h) = doc(&[&[10.0, 50.0, 70.0], &[20.0, 40.0]]);
        h.push_down(&mut d, 0, &[1, 2], 40.0, same).unwrap();
        // 50 → 90 stays; 70 → 110 crosses the edge and lands where page 1's ink began.
        assert_eq!(ys(&d, 0), [10.0, 90.0]);
        // Page 1's ink moves down by the block (1 pt tall) plus the gap; the arrival is
        // appended, so it draws on top.
        let push = 1.0 + SPILL_GAP;
        assert_eq!(ys(&d, 1), [20.0 + push, 40.0 + push, 20.0]);
    }

    #[test]
    fn spilled_block_keeps_its_layout() {
        let (mut d, mut h) = doc(&[&[10.0, 60.0, 80.0], &[]]);
        h.push_down(&mut d, 0, &[1, 2], 50.0, same).unwrap();
        // Both cross; the empty next page takes them at the source page's top margin.
        assert_eq!(ys(&d, 0), [10.0]);
        assert_eq!(ys(&d, 1), [10.0, 30.0]);
    }

    #[test]
    fn cascades_and_adds_a_page_at_the_end() {
        let (mut d, mut h) = doc(&[&[10.0, 90.0], &[10.0, 90.0]]);
        let before: Vec<_> = (0..2).map(|i| ys(&d, i)).collect();
        let c = h.push_down(&mut d, 0, &[1], 20.0, same).unwrap();
        assert!(c.layout);
        assert_eq!(d.page_count(), 3);
        assert_eq!(ys(&d, 0), [10.0]);
        // Page 1: its 10 moves to 29 and arrival lands at 10; its 90 spills to page 2.
        assert_eq!(ys(&d, 1), [10.0 + 1.0 + SPILL_GAP, 10.0]);
        assert_eq!(ys(&d, 2), [10.0]);
        assert_eq!(c.ink.len(), 3);

        // One undo step brings everything back.
        h.undo(&mut d).unwrap().unwrap();
        assert_eq!(d.page_count(), 2);
        assert_eq!((0..2).map(|i| ys(&d, i)).collect::<Vec<_>>(), before);
        h.redo(&mut d).unwrap().unwrap();
        assert_eq!(d.page_count(), 3);
        assert_eq!(ys(&d, 2), [10.0]);
    }

    #[test]
    fn arrivals_never_spill_again() {
        // A stroke taller than a page ends on the next page, over its edge, not in a loop.
        let spec = PageSpec::new((100.0, 100.0), Paper::BLANK);
        let mut d = Document::new(vec![spec; 2]);
        let mut h = History::default();
        let tall = Arc::new(Stroke::new(
            Tool::Pen,
            Rgba8::BLACK,
            1.0,
            [0.0, 150.0].map(|y| Point {
                x: 10.0,
                y,
                pressure: 0.5,
            }),
        ));
        h.add_strokes(&mut d, 0, vec![stroke(5.0), tall]).unwrap();
        h.push_down(&mut d, 0, &[1], 10.0, same).unwrap();
        assert_eq!(d.page_count(), 2);
        assert_eq!(d.pages()[1].strokes().unwrap().len(), 1);
    }

    #[test]
    fn moving_up_never_spills() {
        let (mut d, mut h) = doc(&[&[10.0, 99.0], &[20.0]]);
        h.push_down(&mut d, 0, &[1], -30.0, same).unwrap();
        assert_eq!(ys(&d, 0), [10.0, 69.0]);
        assert_eq!(ys(&d, 1), [20.0]);
    }
}
