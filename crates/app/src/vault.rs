// SPDX-License-Identifier: GPL-3.0-or-later
//! Vaults (DESIGN.md §8): any folder of notes, with per-vault state in a hidden `.cera/` folder
//! (like Obsidian's `.obsidian/`). No GTK here, so it is unit-tested.
//!
//! - `.cera/vault.json`: settings, such as the paper for new notes.
//! - `.cera/workspace.json`: open tabs and the last page of each note.
//! - `~/.config/cera/vaults.json`: recently opened vaults, newest first.
//!
//! JSON throughout (one small dependency); files are replaced atomically.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use cera_core::{PageSpec, Paper, PaperKind, Rgba8};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const STATE_DIR: &str = ".cera";
const MAX_RECENT: usize = 10;
const MM: f32 = 72.0 / 25.4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PageSize {
    A4,
    Letter,
}

/// Paper for new notes and pages.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NewNote {
    pub size: PageSize,
    pub landscape: bool,
    pub paper: PaperKind,
    pub spacing_mm: f32,
}

impl Default for NewNote {
    fn default() -> Self {
        Self {
            size: PageSize::A4,
            landscape: false,
            paper: PaperKind::Ruled,
            spacing_mm: 8.0,
        }
    }
}

impl NewNote {
    pub fn page_spec(&self) -> PageSpec {
        let (w, h) = match self.size {
            PageSize::A4 => PageSpec::A4,
            PageSize::Letter => PageSpec::LETTER,
        };
        let size = if self.landscape { (h, w) } else { (w, h) };
        let paper = Paper {
            kind: self.paper,
            spacing: self.spacing_mm * MM,
            line: Rgba8::new(153, 193, 241, 255),
            fill: Rgba8::WHITE,
        };
        PageSpec::new(size, paper)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VaultConfig {
    pub version: u32,
    pub new_note: NewNote,
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            version: 1,
            new_note: NewNote::default(),
        }
    }
}

/// Session state of a vault. Paths are relative to the vault root, so a vault can move.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Workspace {
    pub tabs: Vec<String>,
    pub active: usize,
    /// Last page (0-based) shown for each note opened in this vault.
    pub last_page: BTreeMap<String, u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Recent {
    pub vaults: Vec<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Vault {
    pub root: PathBuf,
    pub config: VaultConfig,
}

impl Vault {
    /// Opens an existing folder as a vault; `.cera/` is only written when there is state.
    pub fn open(root: &Path) -> io::Result<Self> {
        if !root.is_dir() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "not a folder"));
        }
        let config = read_json(&root.join(STATE_DIR).join("vault.json")).unwrap_or_default();
        Ok(Self {
            root: root.to_owned(),
            config,
        })
    }

    /// Creates the folder (if needed) with a default `.cera/vault.json`.
    pub fn create(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root.join(STATE_DIR))?;
        let vault = Self {
            root: root.to_owned(),
            config: VaultConfig::default(),
        };
        write_json(&root.join(STATE_DIR).join("vault.json"), &vault.config)?;
        Ok(vault)
    }

    pub fn name(&self) -> String {
        self.root.file_name().map_or_else(
            || self.root.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    }

    /// Writes `.cera/vault.json`.
    pub fn save_config(&self) -> io::Result<()> {
        fs::create_dir_all(self.root.join(STATE_DIR))?;
        write_json(&self.root.join(STATE_DIR).join("vault.json"), &self.config)
    }

    pub fn load_workspace(&self) -> Workspace {
        read_json(&self.workspace_path()).unwrap_or_default()
    }

    pub fn save_workspace(&self, ws: &Workspace) -> io::Result<()> {
        fs::create_dir_all(self.root.join(STATE_DIR))?;
        write_json(&self.workspace_path(), ws)
    }

    fn workspace_path(&self) -> PathBuf {
        self.root.join(STATE_DIR).join("workspace.json")
    }

    /// `path` relative to the root, with `/` separators; `None` if it is outside the vault.
    pub fn relative(&self, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(&self.root).ok()?;
        Some(rel.to_string_lossy().into_owned())
    }

    pub fn absolute(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Every folder in the vault (root first, then sorted), for "move to" and import pickers.
    pub fn folders(&self) -> Vec<PathBuf> {
        let mut out = vec![self.root.clone()];
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let mut subdirs: Vec<PathBuf> = fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .map(|e| e.path())
                .collect();
            subdirs.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
            out.extend(subdirs.iter().cloned());
            stack.extend(subdirs);
        }
        out[1..].sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
        out
    }

    /// Notes and PDFs whose file name contains `query` (case-insensitive), sorted.
    pub fn search(&self, query: &str) -> Vec<PathBuf> {
        let query = query.to_lowercase();
        let mut out = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                if !is_listed(&name, is_dir) {
                    continue;
                }
                if is_dir {
                    stack.push(entry.path());
                } else if name.to_lowercase().contains(&query) {
                    out.push(entry.path());
                }
            }
        }
        out.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
        out
    }
}

/// `~/.config/cera` (or under `$XDG_CONFIG_HOME`).
pub fn config_dir() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    config.join("cera")
}

pub fn recent_path() -> PathBuf {
    config_dir().join("vaults.json")
}

/// Recent vaults that still exist, newest first.
pub fn load_recent() -> Recent {
    let mut recent: Recent = read_json(&recent_path()).unwrap_or_default();
    recent.vaults.retain(|p| p.is_dir());
    recent
}

/// Moves `root` to the front of the recent list.
pub fn remember_vault(root: &Path) -> io::Result<()> {
    let mut recent = load_recent();
    recent.vaults.retain(|p| p != root);
    recent.vaults.insert(0, root.to_owned());
    recent.vaults.truncate(MAX_RECENT);
    let path = recent_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    write_json(&path, &recent)
}

/// Shown in the sidebar: folders, notes and PDFs, but no hidden files.
pub fn is_listed(name: &str, is_dir: bool) -> bool {
    if name.starts_with('.') {
        return false;
    }
    let lower = name.to_lowercase();
    is_dir || lower.ends_with(".cera") || lower.ends_with(".pdf")
}

/// `dir/stem.ext`, or `dir/stem 2.ext`, `stem 3`, … if taken. `ext` may be empty (folders).
pub fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let name = |n: u32| {
        let base = if n == 1 {
            stem.to_owned()
        } else {
            format!("{stem} {n}")
        };
        if ext.is_empty() {
            base
        } else {
            format!("{base}.{ext}")
        }
    };
    (1..)
        .map(|n| dir.join(name(n)))
        .find(|p| !p.exists())
        .expect("some name is free")
}

/// Orders names the way people expect: case-insensitive, with digit runs compared as numbers
/// ("Lecture 2" before "Lecture 10").
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a, b);
    loop {
        match (a.chars().next(), b.chars().next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let (na, ra) = split_digits(a);
                let (nb, rb) = split_digits(b);
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
                (a, b) = (ra, rb);
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                (a, b) = (&a[x.len_utf8()..], &b[y.len_utf8()..]);
            }
        }
    }
}

fn split_digits(s: &str) -> (&str, &str) {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s.split_at(end)
}

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(value) => Some(value),
        Err(e) => {
            tracing::warn!(path = %path.display(), "ignoring unreadable settings: {e}");
            None
        }
    }
}

/// Writes JSON to a temporary file next to `path` and renames it over, so a crash leaves
/// either the old or the new file.
pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cera-vault-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn natural_order() {
        let mut v = [
            "Lecture 10",
            "lecture 2",
            "Lecture 1b",
            "Appendix",
            "lecture 02x",
            "Lecture 1",
        ];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            v,
            [
                "Appendix",
                "Lecture 1",
                "Lecture 1b",
                "lecture 2",
                "lecture 02x",
                "Lecture 10"
            ]
        );
    }

    #[test]
    fn listing_rules() {
        assert!(is_listed("Notes", true));
        assert!(is_listed("a.CERA", false));
        assert!(is_listed("slides.pdf", false));
        assert!(!is_listed(".cera", true));
        assert!(!is_listed("todo.txt", false));
    }

    #[test]
    fn unique_names_and_workspace_round_trip() {
        let dir = temp_dir("unique");
        assert_eq!(
            unique_path(&dir, "Untitled", "cera"),
            dir.join("Untitled.cera")
        );
        fs::write(dir.join("Untitled.cera"), b"").unwrap();
        assert_eq!(
            unique_path(&dir, "Untitled", "cera"),
            dir.join("Untitled 2.cera")
        );
        assert_eq!(unique_path(&dir, "New Folder", ""), dir.join("New Folder"));

        let vault = Vault::create(&dir).unwrap();
        assert_eq!(Vault::open(&dir).unwrap().config, VaultConfig::default());
        let mut ws = Workspace::default();
        ws.tabs.push("Physics/Lecture 3.cera".into());
        ws.last_page.insert("Physics/Lecture 3.cera".into(), 4);
        vault.save_workspace(&ws).unwrap();
        assert_eq!(vault.load_workspace(), ws);
        assert_eq!(
            vault.relative(&dir.join("a/b.cera")).as_deref(),
            Some("a/b.cera")
        );
        assert_eq!(vault.relative(Path::new("/elsewhere")), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn search_and_folders_skip_hidden() {
        let dir = temp_dir("search");
        fs::create_dir_all(dir.join("Physics/Labs")).unwrap();
        fs::create_dir_all(dir.join(".cera")).unwrap();
        for f in [
            "Physics/Lecture 2.cera",
            "Physics/Labs/lab lecture.pdf",
            ".cera/lecture.cera",
            "notes.txt",
        ] {
            fs::write(dir.join(f), b"").unwrap();
        }
        let vault = Vault::open(&dir).unwrap();
        let hits: Vec<_> = vault
            .search("LECTURE")
            .iter()
            .map(|p| vault.relative(p).unwrap())
            .collect();
        assert_eq!(
            hits,
            ["Physics/Labs/lab lecture.pdf", "Physics/Lecture 2.cera"]
        );
        let folders: Vec<_> = vault
            .folders()
            .iter()
            .map(|p| vault.relative(p).unwrap())
            .collect();
        assert_eq!(folders, ["", "Physics", "Physics/Labs"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn default_new_note_is_a4_ruled() {
        let spec = NewNote::default().page_spec();
        assert_eq!((spec.width, spec.height), PageSpec::A4);
        assert_eq!(spec.paper.kind, PaperKind::Ruled);
        assert!((spec.paper.spacing - 22.677).abs() < 0.01);
    }
}
