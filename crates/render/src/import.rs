//! PDF → note conversion (DESIGN.md §7, "PDF import"). Nothing is rasterised: the PDF is
//! embedded once and each page gets it as its background.

use std::ops::RangeInclusive;
use std::path::Path;

use cera_core::{AssetKind, Document, MappedBytes, NewAsset, NoteFile, PageSpec};

use crate::pdf::{self, PdfError};

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("cannot read the PDF: {0}")]
    Read(#[from] std::io::Error),
    #[error(transparent)]
    Pdf(#[from] PdfError),
    #[error("cannot write the note: {0}")]
    Write(#[from] cera_core::Error),
    #[error("pages {first}–{last} are not in the PDF, which has {count}", first = .0.start() + 1, last = .0.end() + 1, count = .1)]
    Range(RangeInclusive<u32>, usize),
}

/// Converts the PDF at `pdf` into a note at `out`, replacing any file there. `pages` (0-based,
/// inclusive) picks a range; the whole PDF is still embedded once.
pub fn import_pdf(
    pdf: &Path,
    out: &Path,
    pages: Option<RangeInclusive<u32>>,
) -> Result<(NoteFile, Document), ImportError> {
    let data = MappedBytes::open(pdf)?;
    let info = pdf::inspect(data.clone())?;
    let count = info.pages.len();
    let range = pages.unwrap_or(0..=count.saturating_sub(1) as u32);
    if range.is_empty() || *range.end() as usize >= count {
        return Err(ImportError::Range(range, count));
    }
    let mut doc = Document::new(range.map(|i| PageSpec::pdf(info.pages[i as usize], 0, i)));
    let asset = NewAsset {
        kind: AssetKind::Pdf,
        data: data.as_ref(),
    };
    let file = NoteFile::create(out, &mut doc, &[asset])?;
    Ok((file, doc))
}
