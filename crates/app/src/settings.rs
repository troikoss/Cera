//! App-wide settings in `~/.config/cera/settings.json`: tool presets, touch gestures and the
//! tile cache budget. Per-vault settings (paper for new notes) live in the vault instead.

use std::cell::RefCell;

use cera_core::Rgba8;
use serde::{Deserialize, Serialize};

use crate::canvas::{HIGHLIGHTER_COLORS, HIGHLIGHTER_WIDTHS, PEN_COLORS, PEN_WIDTHS};
use crate::vault;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub pen_colors: [Rgba8; 3],
    pub highlighter_colors: [Rgba8; 3],
    pub pen_width: f32,
    pub highlighter_width: f32,
    pub pressure: bool,
    /// Tap with two fingers to undo, three to redo.
    pub finger_taps: bool,
    /// One finger draws; two fingers scroll and zoom.
    pub draw_with_finger: bool,
    pub tile_budget_mib: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            pen_colors: PEN_COLORS,
            highlighter_colors: HIGHLIGHTER_COLORS,
            pen_width: PEN_WIDTHS[1],
            highlighter_width: HIGHLIGHTER_WIDTHS[1],
            pressure: true,
            finger_taps: true,
            draw_with_finger: false,
            tile_budget_mib: 128,
        }
    }
}

thread_local! {
    static SETTINGS: RefCell<Option<Settings>> = const { RefCell::new(None) };
}

/// The current settings, read from disk the first time.
pub fn get() -> Settings {
    SETTINGS.with(|s| {
        s.borrow_mut()
            .get_or_insert_with(|| {
                vault::read_json(&vault::config_dir().join("settings.json")).unwrap_or_default()
            })
            .clone()
    })
}

/// Reads one value without copying the rest.
pub fn with<T>(f: impl FnOnce(&Settings) -> T) -> T {
    get();
    SETTINGS.with(|s| f(s.borrow().as_ref().expect("loaded by get")))
}

/// Changes the settings and writes them out if anything changed.
pub fn update(f: impl FnOnce(&mut Settings)) {
    let mut settings = get();
    let before = settings.clone();
    f(&mut settings);
    if settings == before {
        return;
    }
    SETTINGS.with(|s| *s.borrow_mut() = Some(settings.clone()));
    let dir = vault::config_dir();
    let written = std::fs::create_dir_all(&dir)
        .and_then(|()| vault::write_json(&dir.join("settings.json"), &settings));
    if let Err(e) = written {
        tracing::warn!("cannot save settings: {e}");
    }
}
