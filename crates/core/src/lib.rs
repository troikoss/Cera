//! Cera's document model, `.cera` file format, geometry and undo.
//!
//! No GTK types live here, so everything is unit-testable without a display.
//! Page coordinates are PDF points (1/72 in).
//!
//! - [`Document`]: pages and their ink, decoded lazily from the file.
//! - [`NoteFile`]: opens, saves (append-only) and compacts a `.cera` file.
//! - [`History`]: the only way to edit ink and pages, with undo/redo.

mod codec;
mod error;
mod file;
mod format;
mod geom;
mod model;
mod simplify;
mod undo;

pub use error::{Error, Result};
pub use file::{AssetInfo, MappedBytes, NewAsset, NoteFile};
pub use format::{AssetKind, FORMAT_VERSION};
pub use geom::{Rect, polygon_contains};
pub use model::{
    AssetId, Background, Document, Page, PageId, PageSpec, Paper, PaperKind, Point, Rgba8, Stroke,
    Tool,
};
pub use simplify::simplify;
pub use undo::{Changes, History};
