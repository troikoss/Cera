//! Undo/redo as a log of diffs. Strokes are `Arc`-shared, so a removed stroke costs a pointer.
//!
//! [`History`] is the only way to edit a [`Document`]'s ink and pages. Commands address
//! strokes and pages by index; that is safe because undo and redo replay strictly in order.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::model::{AssetId, Background, Document, Page, PageId, PageSpec, Stroke};

/// Default number of undo steps kept.
pub const DEFAULT_UNDO_STEPS: usize = 500;

#[derive(Clone, Debug)]
enum Command {
    /// Appended to the end of the page's strokes.
    AddStrokes {
        page: PageId,
        strokes: Vec<Arc<Stroke>>,
    },
    /// Ascending indices into the strokes before removal.
    RemoveStrokes {
        page: PageId,
        removed: Vec<(usize, Arc<Stroke>)>,
    },
    /// In-place changes such as a lasso move or recolour: (index, old, new).
    ReplaceStrokes {
        page: PageId,
        changes: Vec<(usize, Arc<Stroke>, Arc<Stroke>)>,
    },
    InsertPage {
        index: usize,
        page: Page,
    },
    RemovePage {
        index: usize,
        page: Page,
    },
    MovePage {
        from: usize,
        to: usize,
    },
    SetPageSpec {
        index: usize,
        old: PageSpec,
        new: PageSpec,
    },
}

impl Command {
    /// Applies (`forward`) or reverts the command. Checks before it mutates, so a failed run
    /// leaves the document as it was.
    fn run(&self, doc: &mut Document, forward: bool) -> Result<()> {
        match self {
            Command::AddStrokes { page, strokes } => {
                let v = doc.strokes_mut(*page)?;
                if forward {
                    v.extend(strokes.iter().cloned());
                } else {
                    let keep = v.len().checked_sub(strokes.len());
                    v.truncate(keep.ok_or(Error::HistoryMismatch)?);
                }
            }
            Command::RemoveStrokes { page, removed } => {
                let v = doc.strokes_mut(*page)?;
                let last = removed.last().map_or(0, |(i, _)| *i);
                if forward {
                    ensure(last < v.len())?;
                    for (i, _) in removed.iter().rev() {
                        v.remove(*i);
                    }
                } else {
                    ensure(last < v.len() + removed.len())?;
                    for (i, stroke) in removed {
                        v.insert(*i, stroke.clone());
                    }
                }
            }
            Command::ReplaceStrokes { page, changes } => {
                let v = doc.strokes_mut(*page)?;
                ensure(changes.iter().all(|(i, ..)| *i < v.len()))?;
                if forward {
                    for (i, _, new) in changes {
                        v[*i] = new.clone();
                    }
                } else {
                    for (i, old, _) in changes.iter().rev() {
                        v[*i] = old.clone();
                    }
                }
            }
            Command::InsertPage { index, page } => {
                if forward {
                    insert_page(doc, *index, page)?;
                } else {
                    remove_page(doc, *index, page)?;
                }
            }
            Command::RemovePage { index, page } => {
                if forward {
                    remove_page(doc, *index, page)?;
                } else {
                    insert_page(doc, *index, page)?;
                }
            }
            Command::MovePage { from, to } => {
                ensure(*from < doc.page_count() && *to < doc.page_count())?;
                let (from, to) = if forward { (*from, *to) } else { (*to, *from) };
                doc.move_page(from, to);
            }
            Command::SetPageSpec { index, old, new } => {
                ensure(*index < doc.page_count())?;
                doc.set_spec(*index, if forward { *new } else { *old });
            }
        }
        Ok(())
    }

    fn assets(&self) -> impl Iterator<Item = AssetId> {
        let specs = match self {
            Command::InsertPage { page, .. } | Command::RemovePage { page, .. } => {
                [Some(*page.spec()), None]
            }
            Command::SetPageSpec { old, new, .. } => [Some(*old), Some(*new)],
            _ => [None, None],
        };
        specs
            .into_iter()
            .flatten()
            .filter_map(|s| match s.background {
                Background::Pdf { asset, .. } => Some(asset),
                Background::None => None,
            })
    }
}

fn ensure(ok: bool) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error::HistoryMismatch)
    }
}

fn insert_page(doc: &mut Document, index: usize, page: &Page) -> Result<()> {
    ensure(index <= doc.page_count() && doc.page_index(page.id()).is_none())?;
    doc.insert_page(index, page.clone());
    Ok(())
}

fn remove_page(doc: &mut Document, index: usize, page: &Page) -> Result<()> {
    ensure(doc.pages().get(index).is_some_and(|p| p.id() == page.id()))?;
    doc.remove_page(index);
    Ok(())
}

#[derive(Debug)]
pub struct History {
    undo: VecDeque<Command>,
    redo: Vec<Command>,
    max_steps: usize,
}

impl Default for History {
    fn default() -> Self {
        Self::new(DEFAULT_UNDO_STEPS)
    }
}

impl History {
    /// Keeps at most `max_steps` undo steps; older ones are forgotten.
    pub fn new(max_steps: usize) -> Self {
        Self {
            undo: VecDeque::new(),
            redo: Vec::new(),
            max_steps: max_steps.max(1),
        }
    }

    fn push(&mut self, doc: &mut Document, cmd: Command) -> Result<()> {
        cmd.run(doc, true)?;
        self.redo.clear();
        self.undo.push_back(cmd);
        if self.undo.len() > self.max_steps {
            self.undo.pop_front();
        }
        Ok(())
    }

    pub fn add_strokes(
        &mut self,
        doc: &mut Document,
        page: PageId,
        strokes: Vec<Arc<Stroke>>,
    ) -> Result<()> {
        if strokes.is_empty() {
            return Ok(());
        }
        self.push(doc, Command::AddStrokes { page, strokes })
    }

    /// Removes the strokes at `indices` (any order, duplicates ignored).
    pub fn remove_strokes(
        &mut self,
        doc: &mut Document,
        page: PageId,
        indices: &[usize],
    ) -> Result<()> {
        let mut indices = indices.to_vec();
        indices.sort_unstable();
        indices.dedup();
        let current = doc.loaded_strokes(page)?;
        let removed = indices
            .into_iter()
            .map(|i| Ok((i, current.get(i).ok_or(Error::StrokeOutOfRange(i))?.clone())))
            .collect::<Result<Vec<_>>>()?;
        if removed.is_empty() {
            return Ok(());
        }
        self.push(doc, Command::RemoveStrokes { page, removed })
    }

    /// Replaces strokes in place, keeping their order (lasso move, scale, recolour).
    pub fn replace_strokes(
        &mut self,
        doc: &mut Document,
        page: PageId,
        replacements: Vec<(usize, Arc<Stroke>)>,
    ) -> Result<()> {
        let current = doc.loaded_strokes(page)?;
        let changes = replacements
            .into_iter()
            .map(|(i, new)| {
                let old = current.get(i).ok_or(Error::StrokeOutOfRange(i))?.clone();
                Ok((i, old, new))
            })
            .collect::<Result<Vec<_>>>()?;
        if changes.is_empty() {
            return Ok(());
        }
        self.push(doc, Command::ReplaceStrokes { page, changes })
    }

    /// Inserts an empty page before `index` (`index == page_count` appends).
    pub fn insert_page(
        &mut self,
        doc: &mut Document,
        index: usize,
        spec: PageSpec,
    ) -> Result<PageId> {
        if index > doc.page_count() {
            return Err(Error::PageOutOfRange(index));
        }
        let page = doc.new_page(spec);
        let id = page.id();
        self.push(doc, Command::InsertPage { index, page })?;
        Ok(id)
    }

    /// Removes a page; its ink is loaded first so undo can bring it back.
    pub fn remove_page(&mut self, doc: &mut Document, index: usize) -> Result<()> {
        doc.load_ink(index)?;
        let page = doc.pages[index].detached();
        self.push(doc, Command::RemovePage { index, page })
    }

    /// Moves the page at `from` so it ends up at index `to`.
    pub fn move_page(&mut self, doc: &mut Document, from: usize, to: usize) -> Result<()> {
        for i in [from, to] {
            doc.page(i)?;
        }
        if from == to {
            return Ok(());
        }
        self.push(doc, Command::MovePage { from, to })
    }

    pub fn set_page_spec(
        &mut self,
        doc: &mut Document,
        index: usize,
        spec: PageSpec,
    ) -> Result<()> {
        let old = *doc.page(index)?.spec();
        if old == spec {
            return Ok(());
        }
        self.push(
            doc,
            Command::SetPageSpec {
                index,
                old,
                new: spec,
            },
        )
    }

    /// Reverts the newest step; returns `false` if there was nothing to undo.
    pub fn undo(&mut self, doc: &mut Document) -> Result<bool> {
        let Some(cmd) = self.undo.pop_back() else {
            return Ok(false);
        };
        if let Err(e) = cmd.run(doc, false) {
            self.undo.push_back(cmd);
            return Err(e);
        }
        self.redo.push(cmd);
        Ok(true)
    }

    /// Re-applies the newest undone step; returns `false` if there was nothing to redo.
    pub fn redo(&mut self, doc: &mut Document) -> Result<bool> {
        let Some(cmd) = self.redo.pop() else {
            return Ok(false);
        };
        if let Err(e) = cmd.run(doc, true) {
            self.redo.push(cmd);
            return Err(e);
        }
        self.undo.push_back(cmd);
        Ok(true)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }

    /// Assets that undo or redo could bring back; compaction must keep them.
    pub fn referenced_assets(&self) -> impl Iterator<Item = AssetId> + '_ {
        self.undo.iter().chain(&self.redo).flat_map(Command::assets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Paper, PaperKind, Point, Rgba8, Tool};

    fn stroke(n: u8) -> Arc<Stroke> {
        let points = (0..3u8).map(|i| Point {
            x: f32::from(n) + f32::from(i),
            y: 10.0,
            pressure: 0.5,
        });
        Arc::new(Stroke::new(
            Tool::Pen,
            Rgba8::new(n, 0, 0, 255),
            1.0,
            points,
        ))
    }

    type Snapshot = Vec<(PageId, PageSpec, Vec<Arc<Stroke>>)>;

    fn snapshot(doc: &Document) -> Snapshot {
        doc.pages()
            .iter()
            .map(|p| (p.id(), *p.spec(), p.strokes().unwrap().to_vec()))
            .collect()
    }

    fn doc() -> Document {
        Document::new([PageSpec::new(PageSpec::A4, Paper::BLANK); 3])
    }

    /// Applies `edit`, then checks undo restores `before` and redo restores `after`.
    fn check(doc: &mut Document, h: &mut History, edit: impl FnOnce(&mut Document, &mut History)) {
        let before = snapshot(doc);
        edit(doc, h);
        let after = snapshot(doc);
        assert_ne!(before, after, "edit changed nothing");
        assert!(h.undo(doc).unwrap());
        assert_eq!(snapshot(doc), before);
        assert!(h.redo(doc).unwrap());
        assert_eq!(snapshot(doc), after);
    }

    #[test]
    fn stroke_edits_undo_and_redo() {
        let (mut d, mut h) = (doc(), History::default());
        check(&mut d, &mut h, |d, h| {
            h.add_strokes(d, 1, (0..5).map(stroke).collect()).unwrap()
        });
        check(&mut d, &mut h, |d, h| {
            h.remove_strokes(d, 1, &[4, 0, 2, 2]).unwrap()
        });
        assert_eq!(d.pages()[1].strokes().unwrap(), &[stroke(1), stroke(3)]);
        check(&mut d, &mut h, |d, h| {
            h.replace_strokes(d, 1, vec![(1, stroke(9)), (0, stroke(8))])
                .unwrap()
        });
        assert_eq!(d.pages()[1].strokes().unwrap(), &[stroke(8), stroke(9)]);
        assert!(d.pages()[1].is_dirty());
    }

    #[test]
    fn page_edits_undo_and_redo() {
        let (mut d, mut h) = (doc(), History::default());
        h.add_strokes(&mut d, 2, vec![stroke(1)]).unwrap();
        let ruled = PageSpec::new(
            PageSpec::LETTER,
            Paper {
                kind: PaperKind::Ruled,
                spacing: 20.0,
                ..Paper::BLANK
            },
        );
        check(&mut d, &mut h, |d, h| {
            assert_eq!(h.insert_page(d, 3, ruled).unwrap(), 3);
        });
        check(&mut d, &mut h, |d, h| h.remove_page(d, 2).unwrap());
        check(&mut d, &mut h, |d, h| h.move_page(d, 0, 2).unwrap());
        let dots = PageSpec::new(
            PageSpec::A4,
            Paper {
                kind: PaperKind::Dots,
                spacing: 14.0,
                ..Paper::BLANK
            },
        );
        check(&mut d, &mut h, |d, h| h.set_page_spec(d, 1, dots).unwrap());
        // Undo everything back to a single stroke on page 2.
        while h.undo(&mut d).unwrap() {}
        let ids: Vec<_> = d.pages().iter().map(Page::id).collect();
        assert_eq!(ids, [0, 1, 2]);
        assert!(d.pages()[2].strokes().unwrap().is_empty());
    }

    #[test]
    fn new_edit_clears_redo_and_cap_drops_oldest() {
        let (mut d, mut h) = (doc(), History::new(2));
        for n in 0..3 {
            h.add_strokes(&mut d, 0, vec![stroke(n)]).unwrap();
        }
        assert!(h.undo(&mut d).unwrap());
        assert!(h.can_redo());
        h.add_strokes(&mut d, 0, vec![stroke(7)]).unwrap();
        assert!(!h.can_redo());
        assert!(h.undo(&mut d).unwrap());
        assert!(h.undo(&mut d).unwrap());
        assert!(!h.undo(&mut d).unwrap(), "only two steps kept");
        assert_eq!(d.pages()[0].strokes().unwrap(), &[stroke(0)]);
    }

    #[test]
    fn bad_input_is_rejected_without_changes() {
        let (mut d, mut h) = (doc(), History::default());
        let before = snapshot(&d);
        assert!(matches!(
            h.remove_strokes(&mut d, 0, &[0]),
            Err(Error::StrokeOutOfRange(0))
        ));
        assert!(matches!(
            h.add_strokes(&mut d, 99, vec![stroke(0)]),
            Err(Error::UnknownPage(99))
        ));
        assert!(
            h.insert_page(&mut d, 4, PageSpec::new(PageSpec::A4, Paper::BLANK))
                .is_err()
        );
        assert!(h.move_page(&mut d, 0, 3).is_err());
        assert_eq!(snapshot(&d), before);
        assert!(!h.can_undo());
    }

    #[test]
    fn removed_pdf_pages_keep_their_asset() {
        let mut d = Document::new([PageSpec::pdf(PageSpec::A4, 4, 0)]);
        let mut h = History::default();
        h.remove_page(&mut d, 0).unwrap();
        assert_eq!(h.referenced_assets().collect::<Vec<_>>(), [4]);
    }
}
