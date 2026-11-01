// SPDX-License-Identifier: GPL-3.0-or-later
//! Save/open round-trips, crash recovery and compaction on real files.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cera_core::{
    AssetKind, Document, Error, History, NewAsset, NoteFile, PageId, PageSpec, Paper, PaperKind,
    Point, Rgba8, Stroke, Tool,
};

fn temp(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cera-core");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let _ = fs::remove_file(&path);
    path
}

fn stroke(seed: u32, n: usize) -> Arc<Stroke> {
    let points = (0..n).map(|i| {
        let t = i as f32;
        Point {
            x: 50.0 + seed as f32 * 3.0 + t * 0.7,
            y: 80.0 + (t / 5.0 + seed as f32).sin() * 10.0,
            pressure: (i % 10) as f32 / 10.0,
        }
    });
    let tool = if seed.is_multiple_of(3) {
        Tool::Highlighter
    } else {
        Tool::Pen
    };
    Arc::new(Stroke::new(
        tool,
        Rgba8::new(seed as u8, 40, 90, 255),
        1.5,
        points,
    ))
}

/// Everything that should survive a save and reopen.
type Content = Vec<(PageId, PageSpec, Vec<Arc<Stroke>>)>;

fn content(doc: &mut Document) -> Content {
    (0..doc.page_count())
        .map(|i| {
            let strokes = doc.load_ink(i).unwrap().to_vec();
            let page = &doc.pages()[i];
            (page.id(), *page.spec(), strokes)
        })
        .collect()
}

fn reopen(path: &Path) -> (NoteFile, Content) {
    let (file, mut doc) = NoteFile::open(path).unwrap();
    let c = content(&mut doc);
    (file, c)
}

fn fake_pdf() -> Vec<u8> {
    (0..5000u32).map(|i| (i * 7 % 251) as u8).collect()
}

/// A note with a PDF asset and a few saves; returns each generation's content and file length.
fn build_history(path: &Path) -> Vec<(Content, u64)> {
    let pdf = fake_pdf();
    let mut doc = Document::new([
        PageSpec::new(PageSpec::A4, Paper::BLANK),
        PageSpec::pdf((612.0, 792.0), 0, 0),
        PageSpec::pdf((612.0, 792.0), 0, 1),
    ]);
    let mut file = NoteFile::create(
        path,
        &mut doc,
        &[NewAsset {
            kind: AssetKind::Pdf,
            data: &pdf,
        }],
    )
    .unwrap();
    let mut h = History::default();
    let mut generations = vec![(content(&mut doc), file.file_len())];

    h.add_strokes(&mut doc, 0, (0..4).map(|s| stroke(s, 30)).collect())
        .unwrap();
    assert!(file.save(&mut doc).unwrap());
    generations.push((content(&mut doc), file.file_len()));

    h.add_strokes(&mut doc, 2, vec![stroke(7, 50)]).unwrap();
    h.remove_strokes(&mut doc, 0, &[1]).unwrap();
    assert!(file.save(&mut doc).unwrap());
    generations.push((content(&mut doc), file.file_len()));

    let ruled = Paper {
        kind: PaperKind::Ruled,
        spacing: 22.0,
        line: Rgba8::new(150, 180, 220, 255),
        fill: Rgba8::WHITE,
    };
    h.insert_page(&mut doc, 1, PageSpec::new(PageSpec::A4, ruled))
        .unwrap();
    h.move_page(&mut doc, 3, 0).unwrap();
    h.remove_strokes(&mut doc, 0, &[0, 2]).unwrap();
    assert!(file.save(&mut doc).unwrap());
    generations.push((content(&mut doc), file.file_len()));

    assert!(
        !file.save(&mut doc).unwrap(),
        "clean document writes nothing"
    );
    generations
}

#[test]
fn save_and_open_round_trip() {
    let path = temp("round-trip.cera");
    let generations = build_history(&path);
    let (file, c) = reopen(&path);
    assert_eq!(&c, &generations.last().unwrap().0);
    assert_eq!(file.generation(), 4);
    assert_eq!(file.recovered_bytes(), None);
    assert_eq!(file.asset(0).unwrap().as_ref(), &fake_pdf()[..]);
    assert_eq!(
        file.asset(0).unwrap().as_ref().as_ptr() as usize % 4096,
        0,
        "asset is page-aligned"
    );
}

#[test]
fn ink_is_decoded_lazily() {
    let path = temp("lazy.cera");
    build_history(&path);
    let (_file, mut doc) = NoteFile::open(&path).unwrap();
    assert!(
        doc.pages()
            .iter()
            .all(|p| !p.is_loaded() || p.strokes().unwrap().is_empty())
    );
    let index = doc.pages().iter().position(|p| !p.is_loaded()).unwrap();
    assert!(!doc.load_ink(index).unwrap().is_empty());
    assert!(doc.unload_ink(index));
    assert!(!doc.pages()[index].is_loaded());
}

/// Cutting the file at any byte opens the newest save that fits, or fails if none does.
#[test]
fn truncation_at_every_byte_recovers_the_last_complete_save() {
    let path = temp("truncate-source.cera");
    let generations = build_history(&path);
    let bytes = fs::read(&path).unwrap();
    let cut = temp("truncate.cera");
    fs::write(&cut, &bytes).unwrap();
    let file = OpenOptions::new().write(true).open(&cut).unwrap();

    for len in (0..=bytes.len() as u64).rev() {
        file.set_len(len).unwrap();
        let expected = generations.iter().rev().find(|(_, end)| *end <= len);
        match (NoteFile::open(&cut), expected) {
            (Ok((note, mut doc)), Some((want, end))) => {
                assert_eq!(&content(&mut doc), want, "cut at {len}");
                assert_eq!(note.recovered_bytes(), (len > *end).then_some(len - end));
            }
            (Err(_), None) => {}
            (Ok(_), None) => panic!("cut at {len}: opened but no save is complete"),
            (Err(e), Some(_)) => panic!("cut at {len}: {e}"),
        }
    }
}

#[test]
fn torn_save_is_skipped_and_the_next_save_recovers() {
    let path = temp("torn.cera");
    let generations = build_history(&path);
    let garbage = [0x5au8; 100];
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&garbage)
        .unwrap();

    let (mut file, mut doc) = NoteFile::open(&path).unwrap();
    assert_eq!(file.recovered_bytes(), Some(100));
    assert_eq!(&content(&mut doc), &generations.last().unwrap().0);

    let mut h = History::default();
    h.add_strokes(&mut doc, 0, vec![stroke(11, 20)]).unwrap();
    file.save(&mut doc).unwrap();
    let want = content(&mut doc);
    let (file, c) = reopen(&path);
    assert_eq!(c, want);
    assert_eq!(file.recovered_bytes(), None);
}

#[test]
fn damaged_last_footer_falls_back_to_the_previous_save() {
    let path = temp("bad-footer.cera");
    let generations = build_history(&path);
    let mut bytes = fs::read(&path).unwrap();
    let n = bytes.len();
    bytes[n - 10] ^= 0xff;
    fs::write(&path, &bytes).unwrap();
    let (_, c) = reopen(&path);
    assert_eq!(&c, &generations[generations.len() - 2].0);
}

#[test]
fn unknown_chunks_are_skipped() {
    let path = temp("unknown-chunk.cera");
    build_history(&path);
    // A chunk of a kind this build doesn't know, followed by a normal save.
    let payload = b"from the future";
    let mut chunk = vec![0xee];
    chunk.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    let mut crc = crc32fast::Hasher::new();
    crc.update(&chunk);
    crc.update(payload);
    chunk.extend_from_slice(&crc.finalize().to_le_bytes());
    chunk.extend_from_slice(payload);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&chunk)
        .unwrap();

    let (mut file, mut doc) = NoteFile::open(&path).unwrap();
    History::default()
        .add_strokes(&mut doc, 1, vec![stroke(3, 10)])
        .unwrap();
    file.save(&mut doc).unwrap();
    let want = content(&mut doc);
    assert_eq!(reopen(&path).1, want);
}

#[test]
fn compaction_keeps_content_and_drops_dead_bytes() {
    let path = temp("compact.cera");
    let generations = build_history(&path);
    let (mut file, mut doc) = NoteFile::open(&path).unwrap();
    let before = file.file_len();
    assert!(file.dead_bytes() > 0);

    // Leave one page unloaded so its chunk is copied, and one loaded so it is re-encoded.
    doc.load_ink(0).unwrap();
    file.compact(&mut doc, std::iter::empty()).unwrap();
    assert!(file.file_len() < before);
    assert!(file.dead_bytes() < 4096, "only alignment padding is dead");
    assert_eq!(file.generation(), 5);
    assert_eq!(&content(&mut doc), &generations.last().unwrap().0);

    let (file, c) = reopen(&path);
    assert_eq!(&c, &generations.last().unwrap().0);
    assert_eq!(file.asset(0).unwrap().as_ref(), &fake_pdf()[..]);
    assert!(!temp_exists(&path));
}

fn temp_exists(path: &Path) -> bool {
    let name = path.file_name().unwrap().to_string_lossy();
    path.with_file_name(format!(".{name}.tmp")).exists()
}

#[test]
fn compaction_drops_unused_assets_unless_undo_needs_them() {
    let path = temp("assets.cera");
    let (a, b) = (vec![1u8; 100], vec![2u8; 200]);
    let mut doc = Document::new([
        PageSpec::pdf(PageSpec::A4, 0, 0),
        PageSpec::pdf(PageSpec::A4, 1, 0),
    ]);
    let assets = [
        NewAsset {
            kind: AssetKind::Pdf,
            data: &a,
        },
        NewAsset {
            kind: AssetKind::Pdf,
            data: &b,
        },
    ];
    let mut file = NoteFile::create(&path, &mut doc, &assets).unwrap();
    let mut h = History::default();
    h.remove_page(&mut doc, 1).unwrap();
    file.save(&mut doc).unwrap();

    file.compact(&mut doc, h.referenced_assets()).unwrap();
    assert_eq!(
        file.asset(1).unwrap().as_ref(),
        &b[..],
        "undo can still restore page 2"
    );

    h.clear();
    file.compact(&mut doc, h.referenced_assets()).unwrap();
    assert!(file.asset(1).is_none());
    assert_eq!(file.asset(0).unwrap().as_ref(), &a[..]);
    let (file, _) = reopen(&path);
    assert_eq!(file.assets().count(), 1);
}

#[test]
fn restored_page_survives_compaction_in_between() {
    let path = temp("undo-compact.cera");
    let mut doc = Document::new([PageSpec::new(PageSpec::A4, Paper::BLANK); 2]);
    let mut file = NoteFile::create(&path, &mut doc, &[]).unwrap();
    let mut h = History::default();
    h.add_strokes(&mut doc, 1, vec![stroke(1, 40), stroke(2, 40)])
        .unwrap();
    file.save(&mut doc).unwrap();
    let want = content(&mut doc);

    h.remove_page(&mut doc, 1).unwrap();
    file.save(&mut doc).unwrap();
    file.compact(&mut doc, h.referenced_assets()).unwrap();
    h.undo(&mut doc).unwrap();
    file.save(&mut doc).unwrap();
    assert_eq!(reopen(&path).1, want);
}

#[test]
fn rejects_other_files_and_newer_versions() {
    let path = temp("not-a-note.cera");
    fs::write(
        &path,
        b"%PDF-1.7 definitely not a note, but long enough to have a header....",
    )
    .unwrap();
    assert!(matches!(NoteFile::open(&path), Err(Error::NotANote)));

    let path = temp("newer.cera");
    build_history(&path);
    let mut bytes = fs::read(&path).unwrap();
    bytes[8..12].copy_from_slice(&9u32.to_le_bytes());
    let crc = crc32fast::hash(&bytes[..60]);
    bytes[60..64].copy_from_slice(&crc.to_le_bytes());
    fs::write(&path, &bytes).unwrap();
    assert!(matches!(
        NoteFile::open(&path),
        Err(Error::UnsupportedVersion { found: 9, .. })
    ));
}
