// SPDX-License-Identifier: GPL-3.0-or-later
//! Cera's tile rasteriser: Poppler for PDF backgrounds, cairo for ink, and the render worker
//! threads that turn [`worker::TileJob`]s into pixel buffers.
//!
//! No widgets live here. Poppler objects aren't `Send`, so each worker opens its own
//! `poppler::Document`.

pub mod export;
pub mod import;
pub mod ink;
pub mod page;
pub mod pdf;
pub mod tile;
pub mod worker;
