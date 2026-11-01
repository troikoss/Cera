// SPDX-License-Identifier: GPL-3.0-or-later
//! App-wide settings in `~/.config/cera/settings.json`: tool presets, touch gestures and the
//! tile cache budget. Per-vault settings (paper for new notes) live in the vault instead.

use std::cell::RefCell;

use cera_core::Rgba8;
use serde::{Deserialize, Serialize};

use crate::canvas::{
    ERASER_WIDTHS, HIGHLIGHTER_COLORS, HIGHLIGHTER_WIDTHS, MAX_COLOR_SLOTS, MAX_SIZE_SLOTS,
    PEN_COLORS, PEN_WIDTHS,
};
use crate::vault;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    /// Swatch colours, `MAX_COLOR_SLOTS` of them; the first `color_slots` are shown.
    pub pen_colors: Vec<Rgba8>,
    pub highlighter_colors: Vec<Rgba8>,
    /// Size slot widths, `MAX_SIZE_SLOTS` of them; the first `size_slots` are shown.
    pub pen_widths: Vec<f32>,
    pub highlighter_widths: Vec<f32>,
    /// Eraser radii, logical pixels.
    pub eraser_widths: Vec<f32>,
    pub color_slots: usize,
    pub size_slots: usize,
    /// The current widths.
    pub pen_width: f32,
    pub highlighter_width: f32,
    pub eraser_width: f32,
    pub pressure: bool,
    /// The lasso tool draws a rectangle.
    pub rect_select: bool,
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
            pen_colors: PEN_COLORS.to_vec(),
            highlighter_colors: HIGHLIGHTER_COLORS.to_vec(),
            pen_widths: PEN_WIDTHS.to_vec(),
            highlighter_widths: HIGHLIGHTER_WIDTHS.to_vec(),
            eraser_widths: ERASER_WIDTHS.to_vec(),
            color_slots: 3,
            size_slots: 3,
            pen_width: PEN_WIDTHS[1],
            highlighter_width: HIGHLIGHTER_WIDTHS[1],
            eraser_width: ERASER_WIDTHS[1],
            pressure: true,
            rect_select: false,
            finger_taps: true,
            draw_with_finger: false,
            tile_budget_mib: 128,
        }
    }
}

impl Settings {
    /// Pads the slot lists to their full length with the defaults (files from before
    /// 2026.11 have three colours and no widths) and keeps the counts in range.
    fn normalise(&mut self) {
        fn pad<T: Copy>(list: &mut Vec<T>, defaults: &[T]) {
            list.truncate(defaults.len());
            list.extend_from_slice(&defaults[list.len()..]);
        }
        pad(&mut self.pen_colors, &PEN_COLORS);
        pad(&mut self.highlighter_colors, &HIGHLIGHTER_COLORS);
        pad(&mut self.pen_widths, &PEN_WIDTHS);
        pad(&mut self.highlighter_widths, &HIGHLIGHTER_WIDTHS);
        pad(&mut self.eraser_widths, &ERASER_WIDTHS);
        self.color_slots = self.color_slots.clamp(1, MAX_COLOR_SLOTS);
        self.size_slots = self.size_slots.clamp(1, MAX_SIZE_SLOTS);
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
                let mut settings: Settings =
                    vault::read_json(&vault::config_dir().join("settings.json"))
                        .unwrap_or_default();
                settings.normalise();
                settings
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_settings_are_padded() {
        let mut s: Settings = serde_json::from_str(
            r#"{"version":1,"pen_colors":[{"r":1,"g":2,"b":3,"a":255},
                {"r":4,"g":5,"b":6,"a":255},{"r":7,"g":8,"b":9,"a":255}],"pen_width":2.0}"#,
        )
        .unwrap();
        s.normalise();
        assert_eq!(s.pen_colors.len(), MAX_COLOR_SLOTS);
        assert_eq!(s.pen_colors[0], Rgba8::new(1, 2, 3, 255));
        assert_eq!(s.pen_colors[3], PEN_COLORS[3]);
        assert_eq!(s.pen_widths, PEN_WIDTHS);
        assert_eq!((s.color_slots, s.size_slots), (3, 3));
        assert_eq!(s.pen_width, 2.0);
    }

    #[test]
    fn counts_are_clamped() {
        let mut s = Settings {
            color_slots: 0,
            size_slots: 99,
            ..Settings::default()
        };
        s.normalise();
        assert_eq!((s.color_slots, s.size_slots), (1, MAX_SIZE_SLOTS));
    }
}
