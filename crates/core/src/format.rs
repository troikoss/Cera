// SPDX-License-Identifier: GPL-3.0-or-later
//! Container layout of a `.cera` file (DESIGN.md §7).
//!
//! ```text
//! header  64 B  "CERANOTE" version:u32 flags:u32 zero[44] crc32:u32
//! assets        original PDF bytes, each 4 KiB-aligned; offsets are in the manifest
//! chunks        kind:u8 len:u32 crc32:u32 payload[len]   (crc over kind, len, payload)
//! footer  32 B  "CERAFOOT" generation:u64 manifest_offset:u64 manifest_len:u32 crc32:u32
//! ```
//!
//! Integers are little-endian. A save appends changed `PageInk` chunks, a `Manifest` chunk
//! and a footer, so the newest footer is the last 32 bytes of the file. Chunks are only
//! reached through the manifest, so unknown kinds are never looked at.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, corrupt};
use crate::model::{AssetId, Background, PageId, PageSpec};

/// Version of the container layout in the header.
pub const FORMAT_VERSION: u32 = 1;
/// Version of the manifest encoding, its first field. Version 2 added bookmarks; version 1
/// manifests are still read.
pub(crate) const MANIFEST_VERSION: u32 = 2;

pub(crate) const HEADER_LEN: u64 = 64;
pub(crate) const FOOTER_LEN: u64 = 32;
pub(crate) const ASSET_ALIGN: u64 = 4096;
const CHUNK_HEADER_LEN: usize = 9;
const MAGIC: &[u8; 8] = b"CERANOTE";
pub(crate) const FOOTER_MAGIC: &[u8; 8] = b"CERAFOOT";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ChunkKind {
    PageInk = 1,
    // 2 is reserved for Thumbnail (M5).
    Manifest = 3,
}

/// Location of a whole chunk (header included) in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChunkRef {
    pub offset: u64,
    pub len: u32,
}

impl ChunkRef {
    pub fn end(&self) -> u64 {
        self.offset + u64::from(self.len)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssetKind {
    Pdf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AssetEntry {
    pub id: AssetId,
    pub kind: AssetKind,
    pub offset: u64,
    pub len: u64,
    pub crc32: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct PageEntry {
    pub id: PageId,
    pub spec: PageSpec,
    pub ink: Option<ChunkRef>,
}

/// A bookmarked page. An empty name means the page is listed by its number.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BookmarkEntry {
    pub page: PageId,
    pub name: String,
}

/// Postcard-encoded. `version` must stay the first field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub version: u32,
    pub next_page_id: PageId,
    pub assets: Vec<AssetEntry>,
    pub pages: Vec<PageEntry>,
    /// In page order.
    pub bookmarks: Vec<BookmarkEntry>,
}

/// The version 1 manifest: no bookmarks.
#[derive(Deserialize)]
struct ManifestV1 {
    version: u32,
    next_page_id: PageId,
    assets: Vec<AssetEntry>,
    pages: Vec<PageEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Footer {
    pub generation: u64,
    pub manifest: ChunkRef,
}

pub(crate) fn header() -> [u8; HEADER_LEN as usize] {
    let mut h = [0u8; HEADER_LEN as usize];
    h[..8].copy_from_slice(MAGIC);
    h[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    // 12..16: flags, none defined.
    let crc = crc32fast::hash(&h[..60]);
    h[60..].copy_from_slice(&crc.to_le_bytes());
    h
}

pub(crate) fn check_header(data: &[u8]) -> Result<()> {
    let h = data.get(..HEADER_LEN as usize).ok_or(Error::NotANote)?;
    if &h[..8] != MAGIC {
        return Err(Error::NotANote);
    }
    if crc32fast::hash(&h[..60]) != u32_at(h, 60) {
        return Err(corrupt("header checksum"));
    }
    let version = u32_at(h, 8);
    if version > FORMAT_VERSION {
        return Err(Error::UnsupportedVersion {
            what: "format",
            found: version,
            supported: FORMAT_VERSION,
        });
    }
    if version == 0 {
        return Err(corrupt("format version 0"));
    }
    Ok(())
}

pub(crate) fn chunk_header(kind: ChunkKind, payload: &[u8]) -> [u8; CHUNK_HEADER_LEN] {
    let len = u32::try_from(payload.len()).expect("chunks are smaller than 4 GiB");
    let mut h = [0u8; CHUNK_HEADER_LEN];
    h[0] = kind as u8;
    h[1..5].copy_from_slice(&len.to_le_bytes());
    h[5..].copy_from_slice(&chunk_crc(kind as u8, len, payload).to_le_bytes());
    h
}

/// The payload of the chunk at `at`, checked against its kind, length and checksum.
pub(crate) fn read_chunk(data: &[u8], at: ChunkRef, kind: ChunkKind) -> Result<&[u8]> {
    let bytes = usize::try_from(at.offset)
        .ok()
        .and_then(|start| data.get(start..start.checked_add(at.len as usize)?))
        .ok_or_else(|| corrupt("chunk outside the file"))?;
    let (h, payload) = bytes
        .split_first_chunk::<CHUNK_HEADER_LEN>()
        .ok_or_else(|| corrupt("chunk too short"))?;
    let len = u32_at(h, 1);
    if h[0] != kind as u8 || len as usize != payload.len() {
        return Err(corrupt("chunk kind or length mismatch"));
    }
    if chunk_crc(h[0], len, payload) != u32_at(h, 5) {
        return Err(corrupt("chunk checksum"));
    }
    Ok(payload)
}

fn chunk_crc(kind: u8, len: u32, payload: &[u8]) -> u32 {
    let mut crc = crc32fast::Hasher::new();
    crc.update(&[kind]);
    crc.update(&len.to_le_bytes());
    crc.update(payload);
    crc.finalize()
}

pub(crate) fn encode_footer(f: &Footer) -> [u8; FOOTER_LEN as usize] {
    let mut b = [0u8; FOOTER_LEN as usize];
    b[..8].copy_from_slice(FOOTER_MAGIC);
    b[8..16].copy_from_slice(&f.generation.to_le_bytes());
    b[16..24].copy_from_slice(&f.manifest.offset.to_le_bytes());
    b[24..28].copy_from_slice(&f.manifest.len.to_le_bytes());
    let crc = crc32fast::hash(&b[..28]);
    b[28..].copy_from_slice(&crc.to_le_bytes());
    b
}

pub(crate) fn decode_footer(b: &[u8]) -> Option<Footer> {
    let b: &[u8; FOOTER_LEN as usize] = b.try_into().ok()?;
    if &b[..8] != FOOTER_MAGIC || crc32fast::hash(&b[..28]) != u32_at(b, 28) {
        return None;
    }
    Some(Footer {
        generation: u64::from_le_bytes(b[8..16].try_into().unwrap()),
        manifest: ChunkRef {
            offset: u64::from_le_bytes(b[16..24].try_into().unwrap()),
            len: u32_at(b, 24),
        },
    })
}

pub(crate) fn encode_manifest(m: &Manifest) -> Vec<u8> {
    postcard::to_allocvec(m).expect("manifest serialises")
}

pub(crate) fn decode_manifest(payload: &[u8]) -> Result<Manifest> {
    let (version, _) =
        postcard::take_from_bytes::<u32>(payload).map_err(|_| corrupt("manifest version"))?;
    if version > MANIFEST_VERSION {
        return Err(Error::UnsupportedVersion {
            what: "manifest",
            found: version,
            supported: MANIFEST_VERSION,
        });
    }
    let (manifest, rest) = if version == 1 {
        let (m, rest) =
            postcard::take_from_bytes::<ManifestV1>(payload).map_err(|_| corrupt("manifest"))?;
        let m = Manifest {
            version: m.version,
            next_page_id: m.next_page_id,
            assets: m.assets,
            pages: m.pages,
            bookmarks: Vec::new(),
        };
        (m, rest)
    } else {
        postcard::take_from_bytes::<Manifest>(payload).map_err(|_| corrupt("manifest"))?
    };
    if !rest.is_empty() {
        return Err(corrupt("trailing bytes after manifest"));
    }
    validate(&manifest)?;
    Ok(manifest)
}

/// Checks that ids are unique, every PDF background names an existing asset and every
/// bookmark an existing page, once.
pub(crate) fn validate(m: &Manifest) -> Result<()> {
    let mut assets = HashSet::new();
    for a in &m.assets {
        if !assets.insert(a.id) {
            return Err(corrupt(format!("asset {} listed twice", a.id)));
        }
    }
    let mut pages = HashSet::new();
    for p in &m.pages {
        if p.id >= m.next_page_id || !pages.insert(p.id) {
            return Err(corrupt(format!("bad page id {}", p.id)));
        }
        if let Background::Pdf { asset, .. } = p.spec.background
            && !assets.contains(&asset)
        {
            return Err(corrupt(format!("page {} uses missing asset {asset}", p.id)));
        }
    }
    let mut marked = HashSet::new();
    for b in &m.bookmarks {
        if !pages.contains(&b.page) || !marked.insert(b.page) {
            return Err(corrupt(format!("bad bookmark on page {}", b.page)));
        }
    }
    Ok(())
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Paper, PaperKind, Rgba8};

    #[test]
    fn header_and_footer_round_trip() {
        check_header(&header()).unwrap();
        let f = Footer {
            generation: 7,
            manifest: ChunkRef {
                offset: 123_456,
                len: 789,
            },
        };
        let mut b = encode_footer(&f);
        assert_eq!(decode_footer(&b), Some(f));
        b[20] ^= 1;
        assert_eq!(decode_footer(&b), None);
    }

    #[test]
    fn newer_header_is_refused() {
        let mut h = header();
        h[8..12].copy_from_slice(&2u32.to_le_bytes());
        let crc = crc32fast::hash(&h[..60]);
        h[60..].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            check_header(&h),
            Err(Error::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn chunk_checks() {
        let payload = b"hello";
        let mut data = vec![0u8; 3];
        data.extend_from_slice(&chunk_header(ChunkKind::PageInk, payload));
        data.extend_from_slice(payload);
        let at = ChunkRef {
            offset: 3,
            len: (data.len() - 3) as u32,
        };
        assert_eq!(read_chunk(&data, at, ChunkKind::PageInk).unwrap(), payload);
        assert!(read_chunk(&data, at, ChunkKind::Manifest).is_err());
        let short = ChunkRef {
            len: at.len - 1,
            ..at
        };
        assert!(read_chunk(&data, short, ChunkKind::PageInk).is_err());
        *data.last_mut().unwrap() ^= 1;
        assert!(read_chunk(&data, at, ChunkKind::PageInk).is_err());
    }

    fn sample_manifest() -> Manifest {
        let ruled = Paper {
            kind: PaperKind::Ruled,
            spacing: 22.5,
            line: Rgba8::new(10, 20, 30, 40),
            fill: Rgba8::WHITE,
        };
        Manifest {
            version: MANIFEST_VERSION,
            next_page_id: 3,
            assets: vec![AssetEntry {
                id: 0,
                kind: AssetKind::Pdf,
                offset: 4096,
                len: 1000,
                crc32: 0xdead_beef,
            }],
            pages: vec![
                PageEntry {
                    id: 0,
                    spec: PageSpec::pdf((612.0, 792.0), 0, 0),
                    ink: Some(ChunkRef {
                        offset: 5100,
                        len: 300,
                    }),
                },
                PageEntry {
                    id: 2,
                    spec: PageSpec::new((100.0, 200.0), ruled),
                    ink: None,
                },
            ],
            bookmarks: vec![BookmarkEntry {
                page: 2,
                name: "Ch".into(),
            }],
        }
    }

    /// Pins the manifest encoding: if this fails, the file format changed. Bump
    /// MANIFEST_VERSION, keep a reader for the old version, then update the bytes.
    #[test]
    fn manifest_encoding_is_stable() {
        let m = sample_manifest();
        let bytes = encode_manifest(&m);
        assert_eq!(bytes, GOLDEN_MANIFEST, "{bytes:?}");
        assert_eq!(decode_manifest(&bytes).unwrap(), m);
    }

    /// Files written before bookmarks still open, with none.
    #[test]
    fn version_1_manifests_still_open() {
        let m = decode_manifest(GOLDEN_MANIFEST_V1).unwrap();
        let expected = Manifest {
            version: 1,
            bookmarks: Vec::new(),
            ..sample_manifest()
        };
        assert_eq!(m, expected);
    }

    #[rustfmt::skip]
    const GOLDEN_MANIFEST: &[u8] = &[
        2, 3, 1, 0, 0, 128, 32, 232, 7, 239, 253, 182, 245, 13, 2, 0, 0, 0, 25, 68, 0, 0, 70, 68,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255, 1, 0, 0, 1, 236, 39, 172, 2, 2, 0, 0, 200,
        66, 0, 0, 72, 67, 1, 0, 0, 180, 65, 10, 20, 30, 40, 255, 255, 255, 255, 0, 0, 1, 2, 2,
        67, 104,
    ];

    #[rustfmt::skip]
    const GOLDEN_MANIFEST_V1: &[u8] = &[
        1, 3, 1, 0, 0, 128, 32, 232, 7, 239, 253, 182, 245, 13, 2, 0, 0, 0, 25, 68, 0, 0, 70, 68,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255, 1, 0, 0, 1, 236, 39, 172, 2, 2, 0, 0, 200,
        66, 0, 0, 72, 67, 1, 0, 0, 180, 65, 10, 20, 30, 40, 255, 255, 255, 255, 0, 0,
    ];

    #[test]
    fn manifest_validation() {
        let mut m = sample_manifest();
        m.pages[1].id = 0;
        assert!(validate(&m).is_err(), "duplicate page id");
        let mut m = sample_manifest();
        m.next_page_id = 2;
        assert!(validate(&m).is_err(), "id beyond next_page_id");
        let mut m = sample_manifest();
        m.assets.clear();
        assert!(validate(&m).is_err(), "missing asset");
        let mut m = sample_manifest();
        m.bookmarks[0].page = 1;
        assert!(validate(&m).is_err(), "bookmark on a missing page");
        let mut m = sample_manifest();
        m.bookmarks.push(m.bookmarks[0].clone());
        assert!(validate(&m).is_err(), "page bookmarked twice");
        let mut m = sample_manifest();
        m.version = MANIFEST_VERSION + 1;
        assert!(matches!(
            decode_manifest(&encode_manifest(&m)),
            Err(Error::UnsupportedVersion { .. })
        ));
    }
}
