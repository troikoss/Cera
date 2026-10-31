//! Cera's tile rasteriser: cairo for ink, Poppler for PDF backgrounds, and the render worker
//! thread that turns `TileJob`s into RGBA buffers.
//!
//! No widgets live here. Poppler objects aren't `Send`, so each worker opens its own
//! `poppler::Document`. Filled in from M2/M3.
