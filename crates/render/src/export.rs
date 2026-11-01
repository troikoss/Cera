// SPDX-License-Identifier: GPL-3.0-or-later
//! Export (DESIGN.md §7): the whole note as a vector PDF, or one page as a PNG.
//!
//! The PDF keeps everything as vectors: paper as rectangles, PDF backgrounds through
//! Poppler's printing path, strokes as filled paths. Files are written next to the target and
//! renamed over it, so a failed export leaves no half-written file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cera_core::{AssetId, Background, Document, MappedBytes, NoteFile, PageSpec, Stroke};

use crate::page::draw_page;

/// One page to export: its spec and every stroke on it.
#[derive(Clone, Debug)]
pub struct ExportPage {
    pub spec: PageSpec,
    pub strokes: Vec<Arc<Stroke>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("cannot draw: {0}")]
    Cairo(#[from] cairo::Error),
    #[error("cannot write the file: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot write the PNG: {0}")]
    Png(#[from] cairo::IoError),
    #[error("cannot read the note's PDF: {0}")]
    Pdf(#[from] glib::Error),
    #[error("the note has no PDF {0}")]
    MissingAsset(AssetId),
    #[error("the PDF has no page {0}")]
    MissingPage(u32),
    #[error("the page is too large for an image")]
    TooLarge,
}

/// Every page of a note with its ink (decoding pages not loaded yet). The strokes are shared,
/// not copied, so this is cheap and the result can go to another thread.
pub fn note_pages(doc: &mut Document) -> cera_core::Result<Vec<ExportPage>> {
    (0..doc.page_count())
        .map(|i| {
            let strokes = doc.load_ink(i)?.to_vec();
            Ok(ExportPage {
                spec: *doc.page(i)?.spec(),
                strokes,
            })
        })
        .collect()
}

/// The note's embedded PDFs, mapped from its file.
pub fn note_assets(file: &NoteFile) -> HashMap<AssetId, MappedBytes> {
    file.assets()
        .filter_map(|a| Some((a.id, file.asset(a.id)?)))
        .collect()
}

/// Poppler documents opened on demand, one per asset.
struct Backgrounds<'a> {
    assets: &'a HashMap<AssetId, MappedBytes>,
    open: HashMap<AssetId, poppler::Document>,
}

impl Backgrounds<'_> {
    fn page(&mut self, spec: &PageSpec) -> Result<Option<poppler::Page>, ExportError> {
        let Background::Pdf { asset, page } = spec.background else {
            return Ok(None);
        };
        if !self.open.contains_key(&asset) {
            let bytes = self
                .assets
                .get(&asset)
                .ok_or(ExportError::MissingAsset(asset))?;
            let doc = poppler::Document::from_bytes(&glib::Bytes::from_owned(bytes.clone()), None)?;
            self.open.insert(asset, doc);
        }
        let doc = &self.open[&asset];
        doc.page(page as i32)
            .map(Some)
            .ok_or(ExportError::MissingPage(page + 1))
    }
}

/// Writes every page to a PDF at `out`, each at its own size.
pub fn export_pdf(
    out: &Path,
    pages: &[ExportPage],
    assets: &HashMap<AssetId, MappedBytes>,
) -> Result<(), ExportError> {
    let tmp = temp_path(out);
    let result = write_pdf(&tmp, pages, assets);
    finish(result, &tmp, out)
}

fn write_pdf(
    path: &Path,
    pages: &[ExportPage],
    assets: &HashMap<AssetId, MappedBytes>,
) -> Result<(), ExportError> {
    let (w, h) = pages.first().map_or((595.0, 842.0), |p| {
        (f64::from(p.spec.width), f64::from(p.spec.height))
    });
    let surface = cairo::PdfSurface::new(w, h, path)?;
    let mut backgrounds = Backgrounds {
        assets,
        open: HashMap::new(),
    };
    {
        let cr = cairo::Context::new(&surface)?;
        for page in pages {
            surface.set_size(f64::from(page.spec.width), f64::from(page.spec.height))?;
            let pdf = backgrounds.page(&page.spec)?;
            draw_page(&cr, &page.spec, pdf.as_ref(), &page.strokes, true);
            cr.show_page()?;
        }
        cr.status()?;
    }
    surface.finish();
    surface.status()?;
    Ok(())
}

/// Writes one page as a PNG at `scale` pixels per point.
pub fn export_png(
    out: &Path,
    page: &ExportPage,
    assets: &HashMap<AssetId, MappedBytes>,
    scale: f64,
) -> Result<(), ExportError> {
    let px = |pt: f32| (f64::from(pt) * scale).ceil();
    let (w, h) = (px(page.spec.width), px(page.spec.height));
    if w > 16384.0 || h > 16384.0 {
        return Err(ExportError::TooLarge);
    }
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w as i32, h as i32)?;
    let mut backgrounds = Backgrounds {
        assets,
        open: HashMap::new(),
    };
    {
        let cr = cairo::Context::new(&surface)?;
        cr.scale(scale, scale);
        let pdf = backgrounds.page(&page.spec)?;
        draw_page(&cr, &page.spec, pdf.as_ref(), &page.strokes, false);
        cr.status()?;
    }
    let tmp = temp_path(out);
    let result = std::fs::File::create(&tmp)
        .map_err(ExportError::from)
        .and_then(|mut file| Ok(surface.write_to_png(&mut file)?));
    finish(result, &tmp, out)
}

fn temp_path(out: &Path) -> PathBuf {
    let name = out
        .file_name()
        .map_or_else(|| "export".into(), |n| n.to_string_lossy().into_owned());
    out.with_file_name(format!(".{name}.tmp"))
}

/// Renames the finished temp file over `out`, or removes it after a failure.
fn finish(result: Result<(), ExportError>, tmp: &Path, out: &Path) -> Result<(), ExportError> {
    match result {
        Ok(()) => Ok(std::fs::rename(tmp, out)?),
        Err(e) => {
            let _ = std::fs::remove_file(tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cera_core::{Paper, PaperKind, Point, Rgba8, Tool};

    fn page(width: f32) -> ExportPage {
        let line = [(10.0, 10.0), (90.0, 40.0)].map(|(x, y)| Point {
            x,
            y,
            pressure: 1.0,
        });
        ExportPage {
            spec: PageSpec::new(
                (width, 100.0),
                Paper {
                    kind: PaperKind::Grid,
                    spacing: 10.0,
                    line: Rgba8::new(0, 0, 255, 255),
                    fill: Rgba8::WHITE,
                },
            ),
            strokes: vec![Arc::new(Stroke::new(Tool::Pen, Rgba8::BLACK, 2.0, line))],
        }
    }

    #[test]
    fn exports_pdf_pages_at_their_sizes() {
        let dir = std::env::temp_dir().join(format!("cera-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("note.pdf");
        export_pdf(&out, &[page(100.0), page(200.0)], &HashMap::new()).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.starts_with(b"%PDF"));
        let doc = poppler::Document::from_bytes(&glib::Bytes::from_owned(bytes), None).unwrap();
        assert_eq!(doc.n_pages(), 2);
        assert_eq!(doc.page(1).unwrap().size(), (200.0, 100.0));
        assert!(!dir.join(".note.pdf.tmp").exists());

        let png = dir.join("page.png");
        export_png(&png, &page(100.0), &HashMap::new(), 2.0).unwrap();
        let image =
            cairo::ImageSurface::create_from_png(&mut std::fs::File::open(&png).unwrap()).unwrap();
        assert_eq!((image.width(), image.height()), (200, 200));
        std::fs::remove_dir_all(&dir).ok();
    }
}
