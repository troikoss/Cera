//! `cera-cli`: a command-line testing aid for `.cera` files.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use cera_core::{Background, History, NoteFile, PageSpec, Point, Rgba8, Stroke, Tool};

const USAGE: &str = "\
usage: cera-cli <command> [args]

commands:
  import <pdf> <out.cera>      convert a PDF into a note
  info <file.cera>             print pages, assets and file stats
  compact <file.cera>          rewrite the file without dead bytes
  scribble <file.cera> [n]     add n test strokes (default 20) to every page and save";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args[..] {
        ["import", pdf, out] => import(Path::new(pdf), Path::new(out)),
        ["info", file] => info(Path::new(file)),
        ["compact", file] => compact(Path::new(file)),
        ["scribble", file] => scribble(Path::new(file), 20),
        ["scribble", file, n] => match n.parse() {
            Ok(n) => scribble(Path::new(file), n),
            Err(_) => Err(anyhow::anyhow!("not a stroke count: {n}")),
        },
        ["-h" | "--help"] => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        ["-V" | "--version"] => {
            println!("cera-cli {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cera-cli: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn import(pdf: &Path, out: &Path) -> Result<()> {
    if out.exists() {
        bail!("{} already exists", out.display());
    }
    let started = Instant::now();
    let (file, doc) = cera_render::import::import_pdf(pdf, out, None)
        .with_context(|| format!("importing {}", pdf.display()))?;
    println!(
        "imported {} pages into {} ({}) in {:.0?}",
        doc.page_count(),
        out.display(),
        size(file.file_len()),
        started.elapsed()
    );
    Ok(())
}

fn info(path: &Path) -> Result<()> {
    let started = Instant::now();
    let (file, doc) =
        NoteFile::open(path).with_context(|| format!("opening {}", path.display()))?;
    let elapsed = started.elapsed();

    println!("{}", path.display());
    println!("  opened in   {elapsed:.1?} (manifest only)");
    println!(
        "  size        {}, {} dead{}",
        size(file.file_len()),
        size(file.dead_bytes()),
        if file.should_compact() {
            ", compaction due"
        } else {
            ""
        }
    );
    println!("  generation  {}", file.generation());
    if let Some(n) = file.recovered_bytes() {
        println!(
            "  recovered   ignored {} after the last complete save",
            size(n)
        );
    }
    if !file.is_writable() {
        println!("  read-only");
    }
    for a in file.assets() {
        println!("  asset {:<5} {:?}, {}", a.id, a.kind, size(a.len));
    }
    println!("  pages       {}", doc.page_count());
    for (i, page) in doc.pages().iter().enumerate() {
        let s = page.spec();
        let background = match s.background {
            Background::None => format!("{:?} paper", s.paper.kind),
            Background::Pdf { asset, page } => format!("asset {asset} page {}", page + 1),
        };
        let ink = page
            .stored_ink_len()
            .map_or("no ink".into(), |n| format!("ink {}", size(n.into())));
        println!(
            "  {:>5}  {:>6.1} × {:<6.1} pt  {:<12} {background}, {ink}",
            i + 1,
            s.width,
            s.height,
            paper_size_name(s.width, s.height)
        );
    }
    Ok(())
}

fn compact(path: &Path) -> Result<()> {
    let (mut file, mut doc) =
        NoteFile::open(path).with_context(|| format!("opening {}", path.display()))?;
    let before = file.file_len();
    let started = Instant::now();
    file.compact(&mut doc, std::iter::empty())?;
    println!(
        "compacted {}: {} → {} in {:.0?}",
        path.display(),
        size(before),
        size(file.file_len()),
        started.elapsed()
    );
    Ok(())
}

/// Adds wavy test strokes to every page and saves, to exercise the append path.
fn scribble(path: &Path, per_page: u32) -> Result<()> {
    let (mut file, mut doc) =
        NoteFile::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut history = History::default();
    let mut rng = 0x2545_f491_u32;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        rng as f32 / u32::MAX as f32
    };
    let pages: Vec<_> = doc.pages().iter().map(|p| (p.id(), *p.spec())).collect();
    for (id, spec) in pages {
        let strokes = (0..per_page)
            .map(|_| {
                let (x0, y0) = (next() * spec.width * 0.8, next() * spec.height);
                let (len, phase) = (40.0 + next() * 120.0, next() * 6.0);
                let points = (0..80).map(|i| {
                    let t = i as f32 / 79.0;
                    Point {
                        x: x0 + t * len,
                        y: y0 + (t * 12.0 + phase).sin() * 4.0,
                        pressure: 0.3 + 0.5 * (t * 3.0).sin().abs(),
                    }
                });
                Arc::new(Stroke::new(
                    Tool::Pen,
                    Rgba8::new(20, 40, 120, 255),
                    1.4,
                    points,
                ))
            })
            .collect();
        history.add_strokes(&mut doc, id, strokes)?;
    }
    let started = Instant::now();
    file.save(&mut doc)?;
    println!(
        "added {per_page} strokes to each of {} pages; saved in {:.0?}; file now {} ({} dead)",
        doc.page_count(),
        started.elapsed(),
        size(file.file_len()),
        size(file.dead_bytes())
    );
    Ok(())
}

fn paper_size_name(w: f32, h: f32) -> &'static str {
    const SIZES: [(&str, &str, (f32, f32)); 4] = [
        ("A4", "A4 landscape", PageSpec::A4),
        ("Letter", "Letter landsc.", PageSpec::LETTER),
        ("A5", "A5 landscape", (419.53, 595.28)),
        ("A3", "A3 landscape", (841.89, 1190.55)),
    ];
    let near = |a: f32, b: f32| (a - b).abs() < 1.0;
    SIZES
        .iter()
        .find_map(|&(portrait, landscape, (sw, sh))| {
            if near(w, sw) && near(h, sh) {
                Some(portrait)
            } else if near(w, sh) && near(h, sw) {
                Some(landscape)
            } else {
                None
            }
        })
        .unwrap_or("")
}

fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}
