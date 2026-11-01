// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading PDFs with Poppler.

/// What import needs from a PDF.
#[derive(Clone, Debug, PartialEq)]
pub struct PdfInfo {
    /// Page sizes in points, with the page's rotation applied.
    pub pages: Vec<(f32, f32)>,
}

#[derive(Debug, thiserror::Error)]
pub enum PdfError {
    #[error("cannot read PDF: {0}")]
    Poppler(#[from] glib::Error),
    #[error("PDF has no pages")]
    NoPages,
    #[error("PDF page {0} cannot be read")]
    BadPage(usize),
}

/// Page count and sizes. `data` is handed to Poppler without copying (e.g. a
/// `cera_core::MappedBytes`), and nothing is rendered.
pub fn inspect(data: impl AsRef<[u8]> + Send + 'static) -> Result<PdfInfo, PdfError> {
    let bytes = glib::Bytes::from_owned(data);
    let doc = poppler::Document::from_bytes(&bytes, None)?;
    let n = usize::try_from(doc.n_pages()).unwrap_or(0);
    if n == 0 {
        return Err(PdfError::NoPages);
    }
    let pages = (0..n)
        .map(|i| {
            let page = doc.page(i as i32).ok_or(PdfError::BadPage(i))?;
            let (w, h) = page.size();
            Ok((w as f32, h as f32))
        })
        .collect::<Result<_, PdfError>>()?;
    Ok(PdfInfo { pages })
}
