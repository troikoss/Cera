//! Render worker threads: tile jobs in, pixel buffers out.
//!
//! The canvas replaces the whole job queue whenever the viewport changes (visible tiles first,
//! then prefetch), so tiles that scrolled away are never rendered. A worker takes the next job
//! plus queued jobs for the same page and scale, renders their bounding box with one Poppler
//! call (one parse of the page, one decode of a scanned image) and slices it into tiles.
//!
//! Poppler documents aren't `Send`, so each worker opens its own from the mmap'd asset.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Instant;

use cera_core::{AssetId, MappedBytes, Rgba8, Stroke};

use crate::ink;
use crate::tile::{DocId, Grid, PixelRect, TILE_PX, TileKey};

/// Largest area rendered in one Poppler call: 16 tiles, 4 MiB of pixels.
const MAX_BATCH_AREA: u64 = 16 * (TILE_PX as u64) * (TILE_PX as u64);

#[derive(Clone, Debug)]
pub enum JobSource {
    /// Page `page` (0-based) of PDF asset `asset`, painted over `fill` so the tile is opaque.
    Pdf {
        asset: AssetId,
        page: u32,
        fill: Rgba8,
    },
    /// These strokes, in page order, on a transparent tile.
    Ink(Vec<Arc<Stroke>>),
}

#[derive(Clone, Debug)]
pub struct TileJob {
    pub key: TileKey,
    /// Page size in points; with `key.scale` it defines the tile grid.
    pub page_size: (f32, f32),
    /// Returned with the result, so the canvas can spot tiles drawn before a later edit.
    pub generation: u64,
    pub source: JobSource,
}

/// Premultiplied pixels in cairo's ARGB32 layout: B, G, R, A bytes on little-endian machines.
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub bytes: glib::Bytes,
}

pub struct TileResult {
    pub key: TileKey,
    pub generation: u64,
    pub pixels: Result<Pixels, String>,
}

#[derive(Default)]
struct State {
    queue: VecDeque<TileJob>,
    in_flight: HashSet<TileKey>,
    docs: HashMap<DocId, HashMap<AssetId, MappedBytes>>,
    shutdown: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A pool of render threads. Dropping it stops them once their current batch is done.
pub struct Renderer {
    shared: Arc<Shared>,
}

impl Renderer {
    pub fn new(threads: usize, results: async_channel::Sender<TileResult>) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            wake: Condvar::new(),
        });
        for i in 0..threads.max(1) {
            let (shared, results) = (shared.clone(), results.clone());
            thread::Builder::new()
                .name(format!("cera-render-{i}"))
                .spawn(move || run(&shared, &results))
                .expect("spawn render thread");
        }
        Self { shared }
    }

    /// Makes a note's assets available to jobs with this `doc`.
    pub fn open_doc(&self, doc: DocId, assets: impl IntoIterator<Item = (AssetId, MappedBytes)>) {
        self.shared
            .lock()
            .docs
            .insert(doc, assets.into_iter().collect());
    }

    /// Drops a note's queued jobs; workers release its Poppler documents.
    pub fn close_doc(&self, doc: DocId) {
        let mut st = self.shared.lock();
        st.docs.remove(&doc);
        st.queue.retain(|j| j.key.doc != doc);
        drop(st);
        self.shared.wake.notify_all();
    }

    /// Replaces `doc`'s queued jobs, most important first; other notes' jobs stay. Jobs
    /// already rendering are skipped.
    pub fn set_jobs(&self, doc: DocId, jobs: Vec<TileJob>) {
        let mut st = self.shared.lock();
        let State {
            queue,
            in_flight,
            docs,
            ..
        } = &mut *st;
        queue.retain(|j| j.key.doc != doc);
        queue.extend(
            jobs.into_iter()
                .filter(|j| !in_flight.contains(&j.key) && docs.contains_key(&j.key.doc)),
        );
        let wake = !queue.is_empty();
        drop(st);
        if wake {
            self.shared.wake.notify_all();
        }
    }

    pub fn queue_len(&self) -> usize {
        self.shared.lock().queue.len()
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.shared.lock().shutdown = true;
        self.shared.wake.notify_all();
    }
}

fn run(shared: &Shared, results: &async_channel::Sender<TileResult>) {
    let mut pdfs: HashMap<(DocId, AssetId), poppler::Document> = HashMap::new();
    loop {
        let (batch, asset) = {
            let mut st = shared.lock();
            while st.queue.is_empty() && !st.shutdown {
                st = shared.wake.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            if st.shutdown {
                return;
            }
            pdfs.retain(|(doc, _), _| st.docs.contains_key(doc));
            let batch = take_batch(&mut st.queue);
            st.in_flight.extend(batch.iter().map(|j| j.key));
            let first = &batch[0];
            let asset = match &first.source {
                JobSource::Pdf { asset, .. } => st
                    .docs
                    .get(&first.key.doc)
                    .and_then(|assets| assets.get(asset))
                    .cloned(),
                JobSource::Ink(_) => None,
            };
            (batch, asset)
        };

        let started = Instant::now();
        let tiles = render_batch(&batch, asset, &mut pdfs);
        tracing::trace!(
            tiles = batch.len(),
            ms = started.elapsed().as_secs_f64() * 1e3,
            "rendered batch"
        );
        // Send before leaving `in_flight`, so a new queue can't ask for these tiles again.
        for tile in tiles {
            if results.send_blocking(tile).is_err() {
                return;
            }
        }
        let mut st = shared.lock();
        for job in &batch {
            st.in_flight.remove(&job.key);
        }
    }
}

/// The first job plus, for PDF tiles, queued jobs for the same page and scale while their
/// bounding box stays within `MAX_BATCH_AREA`. Ink tiles are cheap and go one at a time.
fn take_batch(queue: &mut VecDeque<TileJob>) -> Vec<TileJob> {
    let first = queue.pop_front().expect("queue is not empty");
    if matches!(first.source, JobSource::Ink(_)) {
        return vec![first];
    }
    let grid = Grid::new(first.page_size, first.key.scale);
    let mut bbox = grid.tile_rect(first.key.tx, first.key.ty);
    let same = |a: &TileJob, b: &TileJob| {
        matches!(a.source, JobSource::Pdf { .. })
            && (a.key.doc, a.key.page, a.key.layer, a.key.scale)
                == (b.key.doc, b.key.page, b.key.layer, b.key.scale)
    };
    let mut batch = vec![first];
    let mut i = 0;
    while i < queue.len() {
        if same(&queue[i], &batch[0]) {
            let merged = bbox.union(grid.tile_rect(queue[i].key.tx, queue[i].key.ty));
            if merged.area() <= MAX_BATCH_AREA {
                bbox = merged;
                batch.extend(queue.remove(i));
                continue;
            }
        }
        i += 1;
    }
    batch
}

fn render_batch(
    batch: &[TileJob],
    asset: Option<MappedBytes>,
    pdfs: &mut HashMap<(DocId, AssetId), poppler::Document>,
) -> Vec<TileResult> {
    let first = &batch[0];
    let grid = Grid::new(first.page_size, first.key.scale);
    let rects: Vec<PixelRect> = batch
        .iter()
        .map(|j| grid.tile_rect(j.key.tx, j.key.ty))
        .collect();
    let bbox = rects
        .iter()
        .copied()
        .reduce(PixelRect::union)
        .expect("batch is not empty");

    let surface = match &first.source {
        JobSource::Pdf {
            asset: id,
            page,
            fill,
        } => (|| {
            let pdf = match pdfs.entry((first.key.doc, *id)) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => {
                    let bytes = asset.ok_or("the note's PDF is not open")?;
                    let doc = poppler::Document::from_bytes(&glib::Bytes::from_owned(bytes), None)
                        .map_err(|e| e.to_string())?;
                    e.insert(doc)
                }
            };
            let page = pdf
                .page(*page as i32)
                .ok_or_else(|| format!("PDF has no page {}", page + 1))?;
            render_page(&page, first, *fill, bbox).map_err(|e| e.to_string())
        })(),
        JobSource::Ink(strokes) => render_ink(strokes, first, bbox).map_err(|e| e.to_string()),
    };

    let surface = match surface {
        Ok(s) => s,
        Err(e) => {
            return batch
                .iter()
                .map(|j| TileResult {
                    key: j.key,
                    generation: j.generation,
                    pixels: Err(e.clone()),
                })
                .collect();
        }
    };

    if batch.len() == 1 {
        // The surface is exactly the tile: hand its buffer over without copying.
        let stride = surface.stride() as usize;
        let pixels = surface.take_data().map(|data| Pixels {
            width: bbox.width,
            height: bbox.height,
            stride,
            bytes: glib::Bytes::from_owned(data),
        });
        return vec![TileResult {
            key: first.key,
            generation: first.generation,
            pixels: pixels.map_err(|e| e.to_string()),
        }];
    }

    let stride = surface.stride() as usize;
    let mut out = Vec::with_capacity(batch.len());
    let copied = surface.with_data(|data| {
        for (job, r) in batch.iter().zip(&rects) {
            let row_len = r.width as usize * 4;
            let mut tile = Vec::with_capacity(row_len * r.height as usize);
            for row in 0..r.height {
                let start = (r.y - bbox.y + row) as usize * stride + (r.x - bbox.x) as usize * 4;
                tile.extend_from_slice(&data[start..start + row_len]);
            }
            out.push(TileResult {
                key: job.key,
                generation: job.generation,
                pixels: Ok(Pixels {
                    width: r.width,
                    height: r.height,
                    stride: row_len,
                    bytes: glib::Bytes::from_owned(tile),
                }),
            });
        }
    });
    if let Err(e) = copied {
        surface.finish();
        return batch
            .iter()
            .map(|j| TileResult {
                key: j.key,
                generation: j.generation,
                pixels: Err(e.to_string()),
            })
            .collect();
    }
    out
}

fn render_page(
    page: &poppler::Page,
    job: &TileJob,
    fill: Rgba8,
    bbox: PixelRect,
) -> Result<cairo::ImageSurface, cairo::Error> {
    let surface =
        cairo::ImageSurface::create(cairo::Format::ARgb32, bbox.width as i32, bbox.height as i32)?;
    let cr = cairo::Context::new(&surface)?;
    let Rgba8 { r, g, b, a } = fill;
    cr.set_source_rgba(
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
        f64::from(a) / 255.0,
    );
    cr.paint()?;
    cr.translate(-f64::from(bbox.x), -f64::from(bbox.y));
    let scale = job.key.scale.get();
    cr.scale(scale, scale);
    page.render(&cr);
    cr.status()?;
    drop(cr);
    surface.flush();
    Ok(surface)
}

fn render_ink(
    strokes: &[Arc<Stroke>],
    job: &TileJob,
    bbox: PixelRect,
) -> Result<cairo::ImageSurface, cairo::Error> {
    let surface =
        cairo::ImageSurface::create(cairo::Format::ARgb32, bbox.width as i32, bbox.height as i32)?;
    let cr = cairo::Context::new(&surface)?;
    cr.translate(-f64::from(bbox.x), -f64::from(bbox.y));
    let scale = job.key.scale.get();
    cr.scale(scale, scale);
    let px = 1.0 / scale;
    let area = cera_core::Rect {
        x0: (f64::from(bbox.x) * px) as f32,
        y0: (f64::from(bbox.y) * px) as f32,
        x1: (f64::from(bbox.x + bbox.width) * px) as f32,
        y1: (f64::from(bbox.y + bbox.height) * px) as f32,
    };
    ink::draw_ink(&cr, strokes, Some(area.expanded(px as f32)));
    cr.status()?;
    drop(cr);
    surface.flush();
    Ok(surface)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::{Layer, Scale};

    fn job(tx: u16, ty: u16, page: u32, scale: f64) -> TileJob {
        TileJob {
            key: TileKey {
                doc: 1,
                page,
                layer: Layer::Pdf,
                scale: Scale::new(scale),
                tx,
                ty,
            },
            page_size: (612.0, 792.0),
            generation: 0,
            source: JobSource::Pdf {
                asset: 0,
                page,
                fill: Rgba8::WHITE,
            },
        }
    }

    #[test]
    fn batches_group_one_page_and_scale_up_to_the_area_limit() {
        let mut queue: VecDeque<_> = [
            job(0, 0, 0, 4.0),
            job(0, 0, 1, 4.0),
            job(1, 0, 0, 4.0),
            job(0, 0, 0, 2.0),
        ]
        .into_iter()
        .chain((0..10).flat_map(|ty| (0..10).map(move |tx| job(tx, ty + 1, 0, 4.0))))
        .collect();
        let batch = take_batch(&mut queue);
        assert!(
            batch
                .iter()
                .all(|j| j.key.page == 0 && j.key.scale == Scale::new(4.0))
        );
        let grid = Grid::new((612.0, 792.0), Scale::new(4.0));
        let bbox = batch
            .iter()
            .map(|j| grid.tile_rect(j.key.tx, j.key.ty))
            .reduce(PixelRect::union)
            .unwrap();
        assert!(bbox.area() <= MAX_BATCH_AREA);
        assert!(batch.len() > 2);
        // Other pages and scales keep their order.
        assert_eq!(queue[0].key.page, 1);
        assert_eq!(queue[1].key.scale, Scale::new(2.0));
    }

    /// Writes a one-page PDF with a black square in the top-left quarter.
    fn write_square_pdf(path: &std::path::Path) {
        let surface = cairo::PdfSurface::new(612.0, 792.0, path).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        cr.rectangle(0.0, 0.0, 306.0, 396.0);
        cr.fill().unwrap();
        drop(cr);
        surface.finish();
    }

    #[test]
    fn renders_pdf_tiles() {
        let dir = std::env::temp_dir().join(format!("cera-render-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("square.pdf");
        write_square_pdf(&path);
        let bytes = MappedBytes::open(&path).unwrap();

        let (tx, rx) = async_channel::unbounded();
        let renderer = Renderer::new(1, tx);
        renderer.open_doc(1, [(0, bytes)]);
        // Scale 2: a 1224 × 1584 grid of 256 px tiles. Ask for a tile inside the square and
        // one outside, plus a neighbour so the batch path (slicing) is used.
        renderer.set_jobs(
            1,
            vec![job(0, 0, 0, 2.0), job(4, 5, 0, 2.0), job(1, 0, 0, 2.0)],
        );
        let mut seen = HashMap::new();
        while seen.len() < 3 {
            let r = rx.recv_blocking().unwrap();
            seen.insert((r.key.tx, r.key.ty), r.pixels.unwrap());
        }
        let first_pixel = |p: &Pixels| p.bytes[..4].to_vec();
        assert_eq!(
            first_pixel(&seen[&(0, 0)]),
            [0, 0, 0, 255],
            "inside the square"
        );
        assert_eq!(
            first_pixel(&seen[&(4, 5)]),
            [255, 255, 255, 255],
            "outside it"
        );
        let edge = &seen[&(4, 5)];
        assert_eq!((edge.width, edge.height), (200, 256));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn renders_ink_tiles_transparent() {
        use cera_core::{Point, Tool};
        let (tx, rx) = async_channel::unbounded();
        let renderer = Renderer::new(1, tx);
        renderer.open_doc(1, []);
        let line = [(10.0, 10.0), (100.0, 10.0)].map(|(x, y)| Point {
            x,
            y,
            pressure: 1.0,
        });
        let stroke = Arc::new(Stroke::new(Tool::Pen, Rgba8::BLACK, 4.0, line));
        let mut ink_job = job(0, 0, 0, 2.0);
        ink_job.key.layer = crate::tile::Layer::Ink;
        ink_job.generation = 7;
        ink_job.source = JobSource::Ink(vec![stroke]);
        renderer.set_jobs(1, vec![ink_job]);
        let r = rx.recv_blocking().unwrap();
        assert_eq!(r.generation, 7);
        let p = r.pixels.unwrap();
        let alpha = |x: usize, y: usize| p.bytes[y * p.stride + x * 4 + 3];
        assert_eq!(alpha(100, 20), 255, "on the line (page y 10 → px 20)");
        assert_eq!(alpha(100, 100), 0, "transparent elsewhere");
    }
}
