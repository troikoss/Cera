//! Cera's tile rasteriser: Poppler for PDF backgrounds (cairo for ink from M4), and the render
//! worker threads that turn [`worker::TileJob`]s into pixel buffers.
//!
//! No widgets live here. Poppler objects aren't `Send`, so each worker opens its own
//! `poppler::Document`.

pub mod import;
pub mod pdf;
pub mod tile;
pub mod worker;
