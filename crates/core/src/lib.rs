// SPDX-License-Identifier: GPL-3.0-or-later
//! Cera's document model, `.cera` file format, geometry and undo.
//!
//! No GTK types live here, so everything is unit-testable without a display.
//! Page coordinates are PDF points (1/72 in).
//!
//! - [`Document`]: pages and their ink, decoded lazily from the file.
//! - [`NoteFile`]: opens, saves (append-only) and compacts a `.cera` file.
//! - [`History`]: the only way to edit ink and pages, with undo/redo.

mod arrange;
mod codec;
mod error;
mod file;
mod format;
mod geom;
mod model;
mod simplify;
mod space;
mod undo;

pub use arrange::{Arrange, arrange};
pub use error::{Error, Result};
pub use file::{AssetInfo, MappedBytes, NewAsset, NoteFile};
pub use format::{AssetKind, FORMAT_VERSION};
pub use geom::{Rect, polygon_contains};
pub use model::{
    AssetId, Background, Document, Layer, Page, PageId, PageSpec, Paper, PaperKind, Pattern, Point,
    Rgba8, Stroke, StrokeStyle,
};
pub use simplify::simplify;
pub use space::{SPILL_GAP, Side};
pub use undo::{Changes, History};
