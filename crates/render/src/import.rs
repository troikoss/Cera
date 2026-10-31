//! PDF → note conversion (DESIGN.md §7, "PDF import"). Nothing is rasterised: the PDF is
//! embedded once and each page gets it as its background.

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
}

/// Converts the PDF at `pdf` into a note at `out`, replacing any file there.
pub fn import_pdf(pdf: &Path, out: &Path) -> Result<(NoteFile, Document), ImportError> {
    let data = MappedBytes::open(pdf)?;
    let info = pdf::inspect(data.clone())?;
    let mut doc = Document::new(
        info.pages
            .iter()
            .zip(0..)
            .map(|(&size, i)| PageSpec::pdf(size, 0, i)),
    );
    let asset = NewAsset {
        kind: AssetKind::Pdf,
        data: data.as_ref(),
    };
    let file = NoteFile::create(out, &mut doc, &[asset])?;
    Ok((file, doc))
}
