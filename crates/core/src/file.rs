// SPDX-License-Identifier: GPL-3.0-or-later
//! Opening, saving and compacting `.cera` files.
//!
//! - **Open** maps the file, reads the footer and manifest, and nothing else; page ink is
//!   decoded later by [`Document::load_ink`]. After a torn write the newest valid footer is
//!   found by scanning backwards; the bytes after it are left alone and counted as dead.
//! - **Save** appends changed ink, a manifest and a footer. The chunks are fsynced before the
//!   footer that points at them is written, so a crash leaves the previous save intact.
//! - **Compact** writes a fresh file next to the old one and renames it over; maps of the old
//!   file stay valid because they keep the old inode.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use memmap2::Mmap;

use crate::codec;
use crate::error::{Error, Result, corrupt};
use crate::format::{
    self, ASSET_ALIGN, AssetEntry, AssetKind, BookmarkEntry, ChunkKind, ChunkRef, FOOTER_LEN,
    FOOTER_MAGIC, Footer, HEADER_LEN, MANIFEST_VERSION, Manifest, PageEntry,
};
use crate::model::{AssetId, Background, Document, Ink};

/// Compaction is due once dead bytes pass this and the live ink + manifest size.
const COMPACT_MIN_DEAD: u64 = 1 << 20;

/// An asset to embed when creating a note.
pub struct NewAsset<'a> {
    pub kind: AssetKind,
    pub data: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssetInfo {
    pub id: AssetId,
    pub kind: AssetKind,
    pub len: u64,
}

/// A read-only slice of a memory-mapped file that keeps the map alive; cheap to clone and
/// `Send`, so it can back a `glib::Bytes` handed to Poppler without copying.
#[derive(Clone)]
pub struct MappedBytes {
    map: Arc<Mmap>,
    start: usize,
    len: usize,
}

impl MappedBytes {
    /// Maps a whole file.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let map = Arc::new(map(&File::open(path)?)?);
        Ok(Self {
            start: 0,
            len: map.len(),
            map,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl AsRef<[u8]> for MappedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.map[self.start..self.start + self.len]
    }
}

impl fmt::Debug for MappedBytes {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "MappedBytes({} bytes)", self.len)
    }
}

fn map(file: &File) -> io::Result<Mmap> {
    // SAFETY: the map is read-only. Another process truncating the file while it is mapped
    // would make reads fault (SIGBUS); we only replace notes by rename, which doesn't.
    unsafe { Mmap::map(file) }
}

/// An open `.cera` file. The pages live in the [`Document`] returned alongside it.
#[derive(Debug)]
pub struct NoteFile {
    path: PathBuf,
    file: File,
    writable: bool,
    map: Arc<Mmap>,
    assets: Vec<AssetEntry>,
    generation: u64,
    len: u64,
    /// Bytes still referenced by the newest footer (header, assets, ink, manifest, footer).
    live: u64,
    /// Bytes after the newest valid footer when the file was opened (a torn write).
    recovered: Option<u64>,
}

impl NoteFile {
    /// Writes `doc` and `assets` as a new note at `path`, replacing any file there. Assets get
    /// ids `0..assets.len()` in order; PDF backgrounds in `doc` must refer to them.
    pub fn create(path: impl AsRef<Path>, doc: &mut Document, assets: &[NewAsset]) -> Result<Self> {
        let assets: Vec<(AssetEntry, &[u8])> = assets
            .iter()
            .zip(0..)
            .map(|(a, id)| {
                let entry = AssetEntry {
                    id,
                    kind: a.kind,
                    offset: 0,
                    len: a.data.len() as u64,
                    crc32: crc32fast::hash(a.data),
                };
                (entry, a.data)
            })
            .collect();
        Self::write_fresh(path.as_ref(), doc, &assets, 1)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<(Self, Document)> {
        let path = path.as_ref();
        let (file, writable) = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => (file, true),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => (File::open(path)?, false),
            Err(e) => return Err(e.into()),
        };
        let map = Arc::new(map(&file)?);
        format::check_header(&map)?;
        let found = find_footer(&map)?;
        let len = map.len() as u64;
        let doc = Document::from_manifest(&found.manifest, map.clone());
        let note = Self {
            path: path.to_owned(),
            file,
            writable,
            assets: found.manifest.assets.clone(),
            generation: found.footer.generation,
            len,
            live: live_bytes(&found.manifest, found.footer.manifest),
            recovered: (found.end < len).then_some(len - found.end),
            map,
        };
        Ok((note, doc))
    }

    /// Appends changed ink, the manifest and a footer; does nothing if `doc` is clean.
    /// Returns whether anything was written.
    pub fn save(&mut self, doc: &mut Document) -> Result<bool> {
        if !doc.is_dirty() {
            return Ok(false);
        }
        if !self.writable {
            return Err(Error::ReadOnly);
        }
        // The real length, not `self.len`: a failed save may have left bytes behind.
        let start = self.file.metadata()?.len();
        let mut w = Appender {
            out: Vec::new(),
            pos: start,
        };
        let mut ink = Vec::with_capacity(doc.pages.len());
        let mut payload = Vec::new();
        for page in &doc.pages {
            ink.push(match &page.ink {
                Ink::Loaded {
                    strokes,
                    dirty: true,
                    ..
                } if !strokes.is_empty() => {
                    payload.clear();
                    codec::encode_page_ink(page.id(), strokes, &mut payload);
                    Some(w.chunk(ChunkKind::PageInk, &payload)?)
                }
                Ink::Loaded { dirty: true, .. } => None,
                _ => page.stored_ref(),
            });
        }
        let manifest = manifest_for(doc, &self.assets, &ink);
        format::validate(&manifest)?;
        let manifest_ref = w.chunk(ChunkKind::Manifest, &format::encode_manifest(&manifest))?;

        self.file.write_all_at(&w.out, start)?;
        self.file.sync_data()?;
        let footer = Footer {
            generation: self.generation + 1,
            manifest: manifest_ref,
        };
        self.file
            .write_all_at(&format::encode_footer(&footer), w.pos)?;
        self.file.sync_data()?;

        self.generation = footer.generation;
        self.len = w.pos + FOOTER_LEN;
        self.live = live_bytes(&manifest, manifest_ref);
        // Remap so ink saved now can be unloaded and decoded again later.
        self.map = Arc::new(map(&self.file)?);
        doc.mark_saved(&ink, self.map.clone());
        Ok(true)
    }

    /// Rewrites the file with only live data. Assets that no page uses are dropped unless
    /// listed in `also_keep` (pass [`crate::History::referenced_assets`] so undo still works).
    pub fn compact(
        &mut self,
        doc: &mut Document,
        also_keep: impl IntoIterator<Item = AssetId>,
    ) -> Result<()> {
        if !self.writable {
            return Err(Error::ReadOnly);
        }
        let mut keep: HashSet<AssetId> = also_keep.into_iter().collect();
        keep.extend(doc.pages.iter().filter_map(|p| match p.spec().background {
            Background::Pdf { asset, .. } => Some(asset),
            Background::None => None,
        }));
        let map = self.map.clone();
        let assets: Vec<(AssetEntry, &[u8])> = self
            .assets
            .iter()
            .filter(|a| keep.contains(&a.id))
            .map(|a| (*a, &map[a.offset as usize..(a.offset + a.len) as usize]))
            .collect();
        *self = Self::write_fresh(&self.path, doc, &assets, self.generation + 1)?;
        Ok(())
    }

    /// Dead bytes outweigh live ink; worth compacting when the note is closed or idle.
    pub fn should_compact(&self) -> bool {
        let asset_bytes: u64 = self.assets.iter().map(|a| a.len).sum();
        let live_log = self.live - HEADER_LEN - asset_bytes;
        let dead = self.dead_bytes();
        dead > COMPACT_MIN_DEAD && dead > live_log
    }

    /// The embedded asset's bytes, straight from the map.
    pub fn asset(&self, id: AssetId) -> Option<MappedBytes> {
        let a = self.assets.iter().find(|a| a.id == id)?;
        Some(MappedBytes {
            map: self.map.clone(),
            start: a.offset as usize,
            len: a.len as usize,
        })
    }

    pub fn assets(&self) -> impl Iterator<Item = AssetInfo> + '_ {
        self.assets.iter().map(|a| AssetInfo {
            id: a.id,
            kind: a.kind,
            len: a.len,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Records that the file was renamed or moved to `path` while open. The open handle and
    /// map follow the inode; only compaction, which writes a new file, uses the path.
    pub fn set_path(&mut self, path: impl Into<PathBuf>) {
        self.path = path.into();
    }

    pub fn file_len(&self) -> u64 {
        self.len
    }

    /// Bytes no longer referenced: superseded ink and manifests, alignment, torn writes.
    pub fn dead_bytes(&self) -> u64 {
        self.len.saturating_sub(self.live)
    }

    /// Incremented by every save and compaction.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Set if the file ended in an incomplete save when it was opened: the number of bytes
    /// after the last complete one, which were ignored.
    pub fn recovered_bytes(&self) -> Option<u64> {
        self.recovered
    }

    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// Writes a complete file to a temporary path, renames it over `path` and reopens it.
    fn write_fresh(
        path: &Path,
        doc: &mut Document,
        assets: &[(AssetEntry, &[u8])],
        generation: u64,
    ) -> Result<Self> {
        let tmp = temp_path(path);
        let written = write_file(&tmp, doc, assets, generation);
        let (manifest, manifest_ref, ink) = match written {
            Ok(w) => w,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        };
        fs::rename(&tmp, path)?;
        sync_dir(path)?;

        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let map = Arc::new(map(&file)?);
        doc.mark_saved(&ink, map.clone());
        Ok(Self {
            path: path.to_owned(),
            file,
            writable: true,
            assets: manifest.assets.clone(),
            generation,
            len: map.len() as u64,
            live: live_bytes(&manifest, manifest_ref),
            recovered: None,
            map,
        })
    }
}

type Written = (Manifest, ChunkRef, Vec<Option<ChunkRef>>);

fn write_file(
    path: &Path,
    doc: &Document,
    assets: &[(AssetEntry, &[u8])],
    generation: u64,
) -> Result<Written> {
    let file = File::create(path)?;
    let mut w = Appender {
        out: BufWriter::new(&file),
        pos: 0,
    };
    w.write(&format::header())?;
    let mut entries = Vec::with_capacity(assets.len());
    for (entry, data) in assets {
        w.pad_to(ASSET_ALIGN)?;
        entries.push(AssetEntry {
            offset: w.pos,
            ..*entry
        });
        w.write(data)?;
    }

    let mut ink = Vec::with_capacity(doc.pages.len());
    let mut payload = Vec::new();
    for page in &doc.pages {
        ink.push(match &page.ink {
            Ink::Stored(r) => {
                let source = doc
                    .source
                    .as_deref()
                    .ok_or_else(|| corrupt("page ink without a file"))?;
                let stored = format::read_chunk(source, *r, ChunkKind::PageInk)?;
                Some(w.chunk(ChunkKind::PageInk, stored)?)
            }
            Ink::Loaded { strokes, .. } if strokes.is_empty() => None,
            Ink::Loaded { strokes, .. } => {
                payload.clear();
                codec::encode_page_ink(page.id(), strokes, &mut payload);
                Some(w.chunk(ChunkKind::PageInk, &payload)?)
            }
        });
    }

    let manifest = manifest_for(doc, &entries, &ink);
    format::validate(&manifest)?;
    let manifest_ref = w.chunk(ChunkKind::Manifest, &format::encode_manifest(&manifest))?;
    w.out.flush()?;
    file.sync_data()?;
    w.write(&format::encode_footer(&Footer {
        generation,
        manifest: manifest_ref,
    }))?;
    w.out.flush()?;
    file.sync_data()?;
    Ok((manifest, manifest_ref, ink))
}

fn manifest_for(doc: &Document, assets: &[AssetEntry], ink: &[Option<ChunkRef>]) -> Manifest {
    Manifest {
        version: MANIFEST_VERSION,
        next_page_id: doc.next_page_id,
        assets: assets.to_vec(),
        pages: doc
            .pages
            .iter()
            .zip(ink)
            .map(|(page, ink)| PageEntry {
                id: page.id(),
                spec: *page.spec(),
                ink: *ink,
            })
            .collect(),
        bookmarks: doc
            .pages
            .iter()
            .filter_map(|page| {
                Some(BookmarkEntry {
                    page: page.id(),
                    name: page.bookmark()?.to_owned(),
                })
            })
            .collect(),
    }
}

fn live_bytes(manifest: &Manifest, manifest_ref: ChunkRef) -> u64 {
    let assets: u64 = manifest.assets.iter().map(|a| a.len).sum();
    let ink: u64 = manifest
        .pages
        .iter()
        .filter_map(|p| p.ink)
        .map(|r| u64::from(r.len))
        .sum();
    HEADER_LEN + assets + ink + u64::from(manifest_ref.len) + FOOTER_LEN
}

struct Found {
    footer: Footer,
    manifest: Manifest,
    /// Where the footer ends.
    end: u64,
}

/// The newest valid footer: normally the last 32 bytes, otherwise found by scanning back.
fn find_footer(data: &[u8]) -> Result<Found> {
    let header = HEADER_LEN as usize;
    let footer_len = FOOTER_LEN as usize;
    if data.len() < header + footer_len {
        return Err(corrupt("file too short"));
    }
    let mut candidate = Some(data.len() - footer_len);
    while let Some(at) = candidate {
        match try_footer(data, at) {
            Ok(found) => return Ok(found),
            // A newer app wrote this; don't fall back to an older save and lose its work.
            Err(e @ Error::UnsupportedVersion { .. }) => return Err(e),
            Err(_) => {}
        }
        // Search strictly before `at`.
        let region = &data[header..(at + FOOTER_MAGIC.len() - 1).min(data.len())];
        candidate = region
            .windows(FOOTER_MAGIC.len())
            .rposition(|w| w == FOOTER_MAGIC)
            .map(|i| header + i);
    }
    Err(corrupt("no complete save found"))
}

fn try_footer(data: &[u8], at: usize) -> Result<Found> {
    let bytes = data
        .get(at..at + FOOTER_LEN as usize)
        .ok_or_else(|| corrupt("footer outside the file"))?;
    let footer = format::decode_footer(bytes).ok_or_else(|| corrupt("footer"))?;
    let at = at as u64;
    let within =
        |offset: u64, end: Option<u64>| offset >= HEADER_LEN && end.is_some_and(|e| e <= at);
    if !within(footer.manifest.offset, Some(footer.manifest.end())) {
        return Err(corrupt("manifest outside the file"));
    }
    let manifest = format::decode_manifest(format::read_chunk(
        data,
        footer.manifest,
        ChunkKind::Manifest,
    )?)?;
    for a in &manifest.assets {
        if !within(a.offset, a.offset.checked_add(a.len)) {
            return Err(corrupt(format!("asset {} outside the file", a.id)));
        }
    }
    for p in &manifest.pages {
        if let Some(r) = p.ink
            && !within(r.offset, Some(r.end()))
        {
            return Err(corrupt(format!("ink of page {} outside the file", p.id)));
        }
    }
    Ok(Found {
        footer,
        manifest,
        end: at + FOOTER_LEN,
    })
}

/// Writes sequentially while tracking the file offset.
struct Appender<W: Write> {
    out: W,
    pos: u64,
}

impl<W: Write> Appender<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    fn pad_to(&mut self, align: u64) -> io::Result<()> {
        let pad = self.pos.next_multiple_of(align) - self.pos;
        self.write(&vec![0; pad as usize])
    }

    fn chunk(&mut self, kind: ChunkKind, payload: &[u8]) -> io::Result<ChunkRef> {
        let offset = self.pos;
        self.write(&format::chunk_header(kind, payload))?;
        self.write(payload)?;
        Ok(ChunkRef {
            offset,
            len: (self.pos - offset) as u32,
        })
    }
}

/// `.name.tmp` next to `path`: same filesystem for the rename, hidden from the vault.
fn temp_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.tmp"))
}

fn sync_dir(path: &Path) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    File::open(dir)?.sync_all()
}
