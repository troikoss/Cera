// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::Arc;

use memmap2::Mmap;
use serde::{Deserialize, Serialize};

use crate::codec;
use crate::error::{Error, Result, corrupt};
use crate::format::{self, ChunkKind, ChunkRef, Manifest};
use crate::geom::Rect;

/// Stable identity of a page within one note; survives reordering.
pub type PageId = u32;
/// Identity of an embedded asset (an original PDF) within one note.
pub type AssetId = u32;

// `Rgba8`, `PaperKind`, `Paper`, `Background` and `PageSpec` are serialised into the manifest
// as they are. Changing them changes the file format: bump `MANIFEST_VERSION` and keep a
// reader for the old layout. The golden test in `format.rs` catches accidental changes.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rgba8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba8 {
    pub const BLACK: Self = Self::new(0, 0, 0, 255);
    pub const WHITE: Self = Self::new(255, 255, 255, 255);
    pub const TRANSPARENT: Self = Self::new(0, 0, 0, 0);

    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}

/// Which pass a stroke is drawn in: highlights under the ink, the ink in page order, then
/// highlights set to draw over it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Layer {
    Under,
    Ink,
    Over,
}

impl Layer {
    /// In drawing order.
    pub const ALL: [Self; 3] = [Self::Under, Self::Ink, Self::Over];
}

/// A stroke's line pattern. Dashes and dots are stored but drawn solid until M9.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pattern {
    Solid,
    Dash,
    Dot,
}

/// How a stroke is drawn, apart from its colour (opacity is the colour's alpha) and width.
/// Four bytes, so strokes barely grow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StrokeStyle {
    pub layer: Layer,
    /// The nib: 0 a square aligned to the page axes, 255 a disc, a rounded square between.
    pub roundness: u8,
    pub pattern: Pattern,
    /// The gap of a dashed or dotted line (and a dash's length) in 1/16 of the width.
    pub spacing: u8,
}

impl StrokeStyle {
    pub const ROUND: u8 = 255;
    /// A round, solid ink stroke: the pen.
    pub const PEN: Self = Self {
        layer: Layer::Ink,
        roundness: Self::ROUND,
        pattern: Pattern::Solid,
        spacing: 16,
    };
    /// A round, solid stroke under the ink: the highlighter.
    pub const HIGHLIGHTER: Self = Self {
        layer: Layer::Under,
        ..Self::PEN
    };

    pub fn is_round(self) -> bool {
        self.roundness == Self::ROUND
    }

    /// Roundness as `0.0..=1.0`.
    pub fn roundness(self) -> f32 {
        f32::from(self.roundness) / 255.0
    }

    /// How far the nib reaches from its centre, as a multiple of its radius: 1 for a disc,
    /// the half-diagonal for a square, in between for a rounded square.
    pub fn reach_factor(self) -> f32 {
        let r = self.roundness();
        r + (1.0 - r) * std::f32::consts::SQRT_2
    }
}

/// One input sample: position in page points, pressure in `0.0..=1.0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
}

/// An immutable stroke; shared as `Arc<Stroke>` between pages, undo and render jobs.
#[derive(Clone, Debug, PartialEq)]
pub struct Stroke {
    style: StrokeStyle,
    color: Rgba8,
    width: f32,
    points: Box<[Point]>,
    bounds: Rect,
}

impl Stroke {
    /// Builds a stroke. Points are rounded to the file's precision (1/64 pt, pressure in
    /// 1/255 steps) so the stroke is identical after a save and reopen. `width` is the
    /// full-pressure width in points.
    ///
    /// Panics if `points` is empty.
    pub fn new(
        style: StrokeStyle,
        color: Rgba8,
        width: f32,
        points: impl IntoIterator<Item = Point>,
    ) -> Self {
        let points: Box<[Point]> = points.into_iter().map(codec::round_point).collect();
        assert!(!points.is_empty(), "a stroke needs at least one point");
        Self::from_rounded(style, color, codec::round_width(width), points)
    }

    /// For values already at file precision (the decoder).
    pub(crate) fn from_rounded(
        style: StrokeStyle,
        color: Rgba8,
        width: f32,
        points: Box<[Point]>,
    ) -> Self {
        let reach = width / 2.0 * style.reach_factor();
        let bounds = Rect::around(points.iter().map(|p| (p.x, p.y))).expanded(reach);
        Self {
            style,
            color,
            width,
            points,
            bounds,
        }
    }

    pub fn style(&self) -> StrokeStyle {
        self.style
    }

    pub fn color(&self) -> Rgba8 {
        self.color
    }

    /// Full-pressure width in points.
    pub fn width(&self) -> f32 {
        self.width
    }

    pub fn points(&self) -> &[Point] {
        &self.points
    }

    /// The area the stroke can paint: its points expanded by the nib's full-pressure reach.
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// The same stroke moved by (`dx`, `dy`) points, rounded to file precision.
    pub fn translated(&self, dx: f32, dy: f32) -> Stroke {
        let points = self.points.iter().map(|p| Point {
            x: p.x + dx,
            y: p.y + dy,
            pressure: p.pressure,
        });
        Stroke::new(self.style, self.color, self.width, points)
    }

    /// The same stroke in another colour.
    pub fn with_color(&self, color: Rgba8) -> Stroke {
        Stroke {
            color,
            ..self.clone()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PaperKind {
    Blank,
    Ruled,
    Grid,
    Dots,
}

/// The page's own pattern, drawn under any PDF background.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Paper {
    pub kind: PaperKind,
    /// Line or dot spacing in points.
    pub spacing: f32,
    pub line: Rgba8,
    pub fill: Rgba8,
}

impl Paper {
    pub const BLANK: Self = Self {
        kind: PaperKind::Blank,
        spacing: 0.0,
        line: Rgba8::TRANSPARENT,
        fill: Rgba8::WHITE,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Background {
    None,
    /// `page` is the 0-based page index within the PDF asset.
    Pdf {
        asset: AssetId,
        page: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PageSpec {
    /// Size in points.
    pub width: f32,
    pub height: f32,
    pub paper: Paper,
    pub background: Background,
}

impl PageSpec {
    /// A4 portrait (210 × 297 mm) in points.
    pub const A4: (f32, f32) = (595.2756, 841.8898);
    /// US Letter portrait in points.
    pub const LETTER: (f32, f32) = (612.0, 792.0);

    pub fn new((width, height): (f32, f32), paper: Paper) -> Self {
        Self {
            width,
            height,
            paper,
            background: Background::None,
        }
    }

    /// A blank page showing page `page` of PDF asset `asset`.
    pub fn pdf((width, height): (f32, f32), asset: AssetId, page: u32) -> Self {
        Self {
            width,
            height,
            paper: Paper::BLANK,
            background: Background::Pdf { asset, page },
        }
    }
}

#[derive(Clone, Debug)]
pub struct Page {
    id: PageId,
    spec: PageSpec,
    /// Set if the page is bookmarked: its name, empty to list it by number.
    bookmark: Option<String>,
    pub(crate) ink: Ink,
}

#[derive(Clone, Debug)]
pub(crate) enum Ink {
    /// In the file, not decoded yet.
    Stored(ChunkRef),
    Loaded {
        strokes: Vec<Arc<Stroke>>,
        /// The chunk holding these strokes, if they have been saved.
        stored: Option<ChunkRef>,
        /// Changed since the last save.
        dirty: bool,
    },
}

impl Page {
    fn new(id: PageId, spec: PageSpec) -> Self {
        Self {
            id,
            spec,
            bookmark: None,
            ink: Ink::Loaded {
                strokes: Vec::new(),
                stored: None,
                dirty: false,
            },
        }
    }

    pub fn id(&self) -> PageId {
        self.id
    }

    pub fn spec(&self) -> &PageSpec {
        &self.spec
    }

    /// The bookmark's name (empty if it has none), or `None` if the page isn't bookmarked.
    pub fn bookmark(&self) -> Option<&str> {
        self.bookmark.as_deref()
    }

    /// The page's strokes, or `None` until [`Document::load_ink`] decodes them.
    pub fn strokes(&self) -> Option<&[Arc<Stroke>]> {
        match &self.ink {
            Ink::Stored(_) => None,
            Ink::Loaded { strokes, .. } => Some(strokes),
        }
    }

    pub fn is_loaded(&self) -> bool {
        matches!(self.ink, Ink::Loaded { .. })
    }

    /// Ink changed since the last save.
    pub fn is_dirty(&self) -> bool {
        matches!(self.ink, Ink::Loaded { dirty: true, .. })
    }

    /// Bytes the page's saved ink takes in the file.
    pub fn stored_ink_len(&self) -> Option<u32> {
        self.stored_ref().map(|r| r.len)
    }

    /// Where the page's ink lives in the file, if the file is up to date for it.
    pub(crate) fn stored_ref(&self) -> Option<ChunkRef> {
        match &self.ink {
            Ink::Stored(r) => Some(*r),
            Ink::Loaded {
                stored,
                dirty: false,
                ..
            } => *stored,
            Ink::Loaded { dirty: true, .. } => None,
        }
    }

    /// A self-contained copy for the undo history: it must not point into the file, which
    /// compaction may rewrite before the page comes back.
    pub(crate) fn detached(&self) -> Page {
        let strokes = self.strokes().expect("detached pages are loaded").to_vec();
        Page {
            id: self.id,
            spec: self.spec,
            bookmark: self.bookmark.clone(),
            ink: Ink::Loaded {
                dirty: !strokes.is_empty(),
                strokes,
                stored: None,
            },
        }
    }
}

#[derive(Debug)]
pub struct Document {
    pub(crate) pages: Vec<Page>,
    pub(crate) next_page_id: PageId,
    /// Page order or specs changed since the last save.
    pub(crate) layout_dirty: bool,
    /// The file that `Ink::Stored` chunks are read from.
    pub(crate) source: Option<Arc<Mmap>>,
}

impl Document {
    /// A new, unsaved document with empty pages.
    pub fn new(specs: impl IntoIterator<Item = PageSpec>) -> Self {
        let pages: Vec<Page> = specs
            .into_iter()
            .zip(0..)
            .map(|(spec, id)| Page::new(id, spec))
            .collect();
        Self {
            next_page_id: pages.len() as PageId,
            pages,
            layout_dirty: true,
            source: None,
        }
    }

    pub(crate) fn from_manifest(manifest: &Manifest, source: Arc<Mmap>) -> Self {
        let pages = manifest
            .pages
            .iter()
            .map(|entry| {
                let mut page = Page::new(entry.id, entry.spec);
                if let Some(r) = entry.ink {
                    page.ink = Ink::Stored(r);
                }
                page.bookmark = manifest
                    .bookmarks
                    .iter()
                    .find(|b| b.page == entry.id)
                    .map(|b| b.name.clone());
                page
            })
            .collect();
        Self {
            pages,
            next_page_id: manifest.next_page_id,
            layout_dirty: false,
            source: Some(source),
        }
    }

    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    pub fn page(&self, index: usize) -> Result<&Page> {
        self.pages.get(index).ok_or(Error::PageOutOfRange(index))
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn page_index(&self, id: PageId) -> Option<usize> {
        self.pages.iter().position(|p| p.id == id)
    }

    /// Unsaved changes in ink, page order or page specs.
    pub fn is_dirty(&self) -> bool {
        self.layout_dirty || self.pages.iter().any(Page::is_dirty)
    }

    /// Decodes a page's ink from the file if it isn't loaded yet.
    pub fn load_ink(&mut self, index: usize) -> Result<&[Arc<Stroke>]> {
        let Self { pages, source, .. } = self;
        let page = pages.get_mut(index).ok_or(Error::PageOutOfRange(index))?;
        if let Ink::Stored(r) = page.ink {
            let data = source
                .as_deref()
                .ok_or_else(|| corrupt("page ink without a file"))?;
            let payload = format::read_chunk(data, r, ChunkKind::PageInk)?;
            let (id, strokes) = codec::decode_page_ink(payload)?;
            if id != page.id {
                return Err(corrupt(format!(
                    "ink of page {id} stored for page {}",
                    page.id
                )));
            }
            page.ink = Ink::Loaded {
                strokes,
                stored: Some(r),
                dirty: false,
            };
        }
        Ok(page.strokes().expect("just loaded"))
    }

    /// Drops a page's decoded strokes if they are saved; returns whether it did.
    pub fn unload_ink(&mut self, index: usize) -> bool {
        let Some(page) = self.pages.get_mut(index) else {
            return false;
        };
        match page.ink {
            Ink::Loaded {
                stored: Some(r),
                dirty: false,
                ..
            } => {
                page.ink = Ink::Stored(r);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn loaded_strokes(&mut self, id: PageId) -> Result<&[Arc<Stroke>]> {
        let index = self.page_index(id).ok_or(Error::UnknownPage(id))?;
        self.load_ink(index)
    }

    /// The page's strokes for editing; marks the page dirty.
    pub(crate) fn strokes_mut(&mut self, id: PageId) -> Result<&mut Vec<Arc<Stroke>>> {
        let index = self.page_index(id).ok_or(Error::UnknownPage(id))?;
        self.load_ink(index)?;
        match &mut self.pages[index].ink {
            Ink::Loaded { strokes, dirty, .. } => {
                *dirty = true;
                Ok(strokes)
            }
            Ink::Stored(_) => unreachable!("loaded above"),
        }
    }

    /// A fresh page with the next free id, not yet in the document.
    pub(crate) fn new_page(&mut self, spec: PageSpec) -> Page {
        let page = Page::new(self.next_page_id, spec);
        self.next_page_id += 1;
        self.layout_dirty = true;
        page
    }

    pub(crate) fn insert_page(&mut self, index: usize, page: Page) {
        self.pages.insert(index, page);
        self.layout_dirty = true;
    }

    pub(crate) fn remove_page(&mut self, index: usize) -> Page {
        self.layout_dirty = true;
        self.pages.remove(index)
    }

    pub(crate) fn move_page(&mut self, from: usize, to: usize) {
        let page = self.pages.remove(from);
        self.pages.insert(to, page);
        self.layout_dirty = true;
    }

    /// Bookmarks page `index` (`Some(name)`, empty for none) or removes its bookmark. Not part
    /// of the undo history, like a bookmark in a book; a page brought back by undo keeps its
    /// bookmark.
    pub fn set_bookmark(&mut self, index: usize, bookmark: Option<String>) -> Result<()> {
        let page = self
            .pages
            .get_mut(index)
            .ok_or(Error::PageOutOfRange(index))?;
        if page.bookmark != bookmark {
            page.bookmark = bookmark;
            self.layout_dirty = true;
        }
        Ok(())
    }

    /// Indices of the bookmarked pages, in order.
    pub fn bookmarks(&self) -> impl Iterator<Item = (usize, &Page)> {
        self.pages
            .iter()
            .enumerate()
            .filter(|(_, p)| p.bookmark.is_some())
    }

    pub(crate) fn set_spec(&mut self, index: usize, spec: PageSpec) {
        self.pages[index].spec = spec;
        self.layout_dirty = true;
    }

    /// After a save: `ink[i]` is where page `i`'s ink now lives.
    pub(crate) fn mark_saved(&mut self, ink: &[Option<ChunkRef>], source: Arc<Mmap>) {
        for (page, new_ref) in self.pages.iter_mut().zip(ink) {
            match &mut page.ink {
                Ink::Stored(r) => *r = new_ref.expect("stored ink is always written"),
                Ink::Loaded { stored, dirty, .. } => {
                    *stored = *new_ref;
                    *dirty = false;
                }
            }
        }
        self.layout_dirty = false;
        self.source = Some(source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_and_recolour_keep_the_rest() {
        let points =
            [(1.0, 2.0, 0.5), (3.0, 5.0, 1.0)].map(|(x, y, pressure)| Point { x, y, pressure });
        let s = Stroke::new(StrokeStyle::HIGHLIGHTER, Rgba8::BLACK, 4.0, points);
        let moved = s.translated(10.0, -1.0);
        assert_eq!(
            (moved.style(), moved.color(), moved.width()),
            (s.style(), s.color(), s.width())
        );
        assert_eq!(
            moved.points()[1],
            Point {
                x: 13.0,
                y: 4.0,
                pressure: 1.0
            }
        );
        assert_eq!(moved.bounds(), s.bounds().translated(10.0, -1.0));
        let red = s.with_color(Rgba8::new(255, 0, 0, 90));
        assert_eq!(red.points(), s.points());
        assert_eq!(red.color(), Rgba8::new(255, 0, 0, 90));
    }
}
