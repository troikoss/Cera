//! Tile textures with a byte budget, evicting the least recently drawn first.
//!
//! Generic over the texture type so the bookkeeping is testable without a display.

use std::collections::HashMap;

use cera_core::PageId;
use cera_render::tile::{DocId, Layer, Scale, TileKey};

pub type PageKey = (DocId, PageId, Layer);

struct Entry<T> {
    value: T,
    bytes: usize,
    /// Frame in which the tile was last drawn or inserted; tiles of the current frame stay.
    frame: u64,
    /// Order of last use, for least-recently-used eviction.
    stamp: u64,
}

pub struct TileCache<T> {
    tiles: HashMap<TileKey, Entry<T>>,
    /// Scales held per page and how many tiles each, to find fallbacks during zoom.
    scales: HashMap<PageKey, Vec<(Scale, usize)>>,
    bytes: usize,
    budget: usize,
    frame: u64,
    stamp: u64,
}

fn page_key(k: &TileKey) -> PageKey {
    (k.doc, k.page, k.layer)
}

impl<T> TileCache<T> {
    pub fn new(budget: usize) -> Self {
        Self {
            tiles: HashMap::new(),
            scales: HashMap::new(),
            bytes: 0,
            budget,
            frame: 1,
            stamp: 0,
        }
    }

    /// Starts a frame: tiles touched from now on count as on screen and are not evicted.
    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    pub fn contains(&self, key: &TileKey) -> bool {
        self.tiles.contains_key(key)
    }

    /// The tile, marked as drawn in this frame.
    pub fn get(&mut self, key: &TileKey) -> Option<&T> {
        self.stamp += 1;
        let (frame, stamp) = (self.frame, self.stamp);
        self.tiles.get_mut(key).map(|e| {
            e.frame = frame;
            e.stamp = stamp;
            &e.value
        })
    }

    pub fn insert(&mut self, key: TileKey, value: T, bytes: usize) {
        self.stamp += 1;
        let entry = Entry {
            value,
            bytes,
            frame: self.frame,
            stamp: self.stamp,
        };
        match self.tiles.insert(key, entry) {
            Some(old) => self.bytes -= old.bytes,
            None => self.count_scale(&key, 1),
        }
        self.bytes += bytes;
        self.evict();
    }

    /// Scales this page has tiles at.
    pub fn scales(&self, page: &PageKey) -> impl Iterator<Item = Scale> + '_ {
        self.scales.get(page).into_iter().flatten().map(|&(s, _)| s)
    }

    /// Low-memory response: keep only tiles drawn in the current frame.
    pub fn retain_current_frame(&mut self) {
        let frame = self.frame;
        self.remove_where(|_, e| e.frame < frame);
    }

    /// Drops a page's tiles at every scale but `keep`, once `keep` covers what's on screen.
    pub fn remove_other_scales(&mut self, page: &PageKey, keep: Scale) {
        if self.scales.get(page).is_some_and(|s| s.len() > 1) {
            self.remove_where(|k, _| page_key(k) == *page && k.scale != keep);
        }
    }

    pub fn remove_doc(&mut self, doc: DocId) {
        self.remove_where(|k, _| k.doc == doc);
    }

    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Drops least recently drawn tiles until within budget, never ones from this frame.
    fn evict(&mut self) {
        while self.bytes > self.budget {
            let frame = self.frame;
            let oldest = self
                .tiles
                .iter()
                .filter(|(_, e)| e.frame < frame)
                .min_by_key(|(_, e)| e.stamp)
                .map(|(k, _)| *k);
            let Some(key) = oldest else { break };
            self.remove(&key);
        }
    }

    fn remove(&mut self, key: &TileKey) {
        if let Some(e) = self.tiles.remove(key) {
            self.bytes -= e.bytes;
            self.count_scale(key, -1);
        }
    }

    fn remove_where(&mut self, pred: impl Fn(&TileKey, &Entry<T>) -> bool) {
        let doomed: Vec<TileKey> = self
            .tiles
            .iter()
            .filter(|(k, e)| pred(k, e))
            .map(|(k, _)| *k)
            .collect();
        for key in doomed {
            self.remove(&key);
        }
    }

    fn count_scale(&mut self, key: &TileKey, delta: isize) {
        let page = page_key(key);
        let scales = self.scales.entry(page).or_default();
        match scales.iter_mut().find(|(s, _)| *s == key.scale) {
            Some((_, n)) => *n = n.saturating_add_signed(delta),
            None if delta > 0 => scales.push((key.scale, delta as usize)),
            None => {}
        }
        scales.retain(|&(_, n)| n > 0);
        if scales.is_empty() {
            self.scales.remove(&page);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(page: PageId, scale: f64, tx: u16) -> TileKey {
        TileKey {
            doc: 1,
            page,
            layer: Layer::Pdf,
            scale: Scale::new(scale),
            tx,
            ty: 0,
        }
    }

    #[test]
    fn evicts_least_recently_drawn_within_budget() {
        let mut c = TileCache::new(300);
        for tx in 0..3 {
            c.insert(key(0, 1.0, tx), tx, 100);
        }
        c.begin_frame();
        c.get(&key(0, 1.0, 0)); // drawn now
        c.insert(key(0, 1.0, 3), 3, 100);
        assert_eq!(c.bytes(), 300);
        assert!(c.contains(&key(0, 1.0, 0)), "drawn this frame");
        assert!(!c.contains(&key(0, 1.0, 1)), "oldest evicted");
        assert!(c.contains(&key(0, 1.0, 3)));
    }

    #[test]
    fn never_evicts_what_is_on_screen() {
        let mut c = TileCache::new(100);
        c.begin_frame();
        c.insert(key(0, 1.0, 0), 0, 100);
        c.insert(key(0, 1.0, 1), 1, 100);
        assert_eq!(c.len(), 2, "both inserted this frame");
        c.begin_frame();
        c.retain_current_frame();
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn tracks_scales_per_page() {
        let mut c = TileCache::new(10_000);
        c.insert(key(0, 1.0, 0), (), 10);
        c.insert(key(0, 2.0, 0), (), 10);
        c.insert(key(0, 2.0, 1), (), 10);
        c.insert(key(5, 2.0, 0), (), 10);
        let mut scales: Vec<f64> = c.scales(&(1, 0, Layer::Pdf)).map(Scale::get).collect();
        scales.sort_by(f64::total_cmp);
        assert_eq!(scales, [1.0, 2.0]);
        c.remove_other_scales(&(1, 0, Layer::Pdf), Scale::new(2.0));
        assert_eq!(c.len(), 3);
        assert!(!c.contains(&key(0, 1.0, 0)));
        c.remove_doc(1);
        assert_eq!(c.scales(&(1, 0, Layer::Pdf)).count(), 0);
        assert_eq!((c.len(), c.bytes()), (0, 0));
    }
}
