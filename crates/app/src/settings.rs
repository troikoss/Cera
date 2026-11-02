// SPDX-License-Identifier: GPL-3.0-or-later
//! App-wide settings in `~/.config/cera/settings.json`: tool presets, the tool bar, touch
//! gestures and the tile cache budget. Per-vault settings (paper for new notes) live in the
//! vault instead.

use std::cell::RefCell;

use cera_core::Rgba8;
use gtk::glib;
use serde::{Deserialize, Serialize};

use crate::preset::{
    ERASER_WIDTHS, HIGHLIGHTER_WIDTHS, MAX_COLOR_SLOTS, MAX_SIZE_SLOTS, PEN_COLORS, PEN_WIDTHS,
    Preset, PresetId, Secondary,
};
use crate::vault;

/// Settings files from before tool presets (2026.11) are version 1.
const VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    /// Every preset, in the order the preferences list them.
    pub presets: Vec<Preset>,
    /// The presets in the tool bar, in order; 1–9 pick the first nine.
    pub favorites: Vec<PresetId>,
    /// The preset last picked in the tool bar.
    pub active: PresetId,
    /// What the stylus's eraser end and barrel button do.
    pub secondary: Secondary,
    pub next_preset_id: PresetId,
    /// How many swatches and size slots the tool bar shows.
    pub color_slots: usize,
    pub size_slots: usize,
    /// Which window edge the floating tool bar sits on.
    pub toolbar_position: ToolbarPosition,
    /// What the lasso tool draws to select.
    pub select_shape: SelectShape,
    /// Tap with two fingers to undo, three to redo.
    pub finger_taps: bool,
    /// One finger draws; two fingers scroll and zoom.
    pub draw_with_finger: bool,
    pub tile_budget_mib: u32,
    /// Colours picked from the wheel or typed in, newest first, shown under the palette.
    pub recent_colors: Vec<Rgba8>,
}

/// How many recent colours are kept: one row of the palette.
pub const MAX_RECENT_COLORS: usize = 6;

/// The per-tool fields of version 1, read once to make the default presets from them.
#[derive(Deserialize)]
#[serde(default)]
struct Version1 {
    pen_colors: Vec<Rgba8>,
    highlighter_colors: Vec<Rgba8>,
    pen_widths: Vec<f32>,
    highlighter_widths: Vec<f32>,
    eraser_widths: Vec<f32>,
    pen_width: f32,
    highlighter_width: f32,
    eraser_width: f32,
    pressure: bool,
}

impl Default for Version1 {
    fn default() -> Self {
        Self {
            pen_colors: PEN_COLORS.to_vec(),
            highlighter_colors: Vec::new(),
            pen_widths: PEN_WIDTHS.to_vec(),
            highlighter_widths: HIGHLIGHTER_WIDTHS.to_vec(),
            eraser_widths: ERASER_WIDTHS.to_vec(),
            pen_width: PEN_WIDTHS[1],
            highlighter_width: HIGHLIGHTER_WIDTHS[1],
            eraser_width: ERASER_WIDTHS[1],
            pressure: true,
        }
    }
}

impl Version1 {
    /// The three default presets with the old tools' slots. Highlighter colours lose their
    /// alpha, which becomes the preset's opacity.
    fn presets(self) -> Vec<Preset> {
        let mut presets = Preset::defaults();
        let [pen, highlighter, eraser] = &mut presets[..] else {
            unreachable!("three defaults")
        };
        pen.colors = self.pen_colors;
        pen.widths = self.pen_widths;
        pen.width = self.pen_width;
        pen.pressure = self.pressure;
        if let Some(first) = self.highlighter_colors.first() {
            highlighter.opacity = f32::from(first.a) / 255.0;
        }
        highlighter.colors = self
            .highlighter_colors
            .iter()
            .map(|c| Rgba8 { a: 255, ..*c })
            .collect();
        highlighter.widths = self.highlighter_widths;
        highlighter.width = self.highlighter_width;
        eraser.widths = self.eraser_widths;
        eraser.width = self.eraser_width;
        for p in &mut presets {
            p.normalise();
            p.color = p.colors[0];
        }
        presets
    }
}

/// The window edge the tool bar floats along.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolbarPosition {
    #[default]
    Bottom,
    Top,
    Left,
    Right,
}

impl ToolbarPosition {
    /// In the order the preferences list them.
    pub const ALL: [Self; 4] = [Self::Bottom, Self::Top, Self::Left, Self::Right];

    pub fn label(self) -> &'static str {
        match self {
            Self::Bottom => "Bottom",
            Self::Top => "Top",
            Self::Left => "Left",
            Self::Right => "Right",
        }
    }

    pub fn is_vertical(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }
}

/// How the lasso tool selects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SelectShape {
    /// Strokes mostly inside a free loop.
    #[default]
    Lasso,
    /// Strokes mostly inside a dragged rectangle.
    Rectangle,
    /// Strokes the drag passes over, or the one tapped.
    Stroke,
    /// Everything below the press, dragged up or down to remove or insert space.
    Space,
}

impl Default for Settings {
    fn default() -> Self {
        let presets = Preset::defaults();
        Self {
            version: VERSION,
            favorites: presets.iter().map(|p| p.id).collect(),
            active: presets[0].id,
            secondary: Secondary::default(),
            next_preset_id: presets.len() as PresetId,
            presets,
            color_slots: 3,
            size_slots: 3,
            toolbar_position: ToolbarPosition::Bottom,
            select_shape: SelectShape::Lasso,
            finger_taps: true,
            draw_with_finger: false,
            tile_budget_mib: 128,
            recent_colors: Vec::new(),
        }
    }
}

impl Settings {
    /// Reads a settings file, turning a version 1 file's tools into presets.
    fn parse(json: &str) -> serde_json::Result<Self> {
        let mut settings: Settings = serde_json::from_str(json)?;
        if settings.version < VERSION {
            let old: Version1 = serde_json::from_str(json)?;
            let defaults = Settings::default();
            settings.presets = old.presets();
            settings.favorites = defaults.favorites;
            settings.active = defaults.active;
            settings.secondary = defaults.secondary;
            settings.next_preset_id = defaults.next_preset_id;
            settings.version = VERSION;
        }
        settings.normalise();
        Ok(settings)
    }

    /// Pads the presets' slot lists, keeps counts in range and ids pointing at presets.
    fn normalise(&mut self) {
        if self.presets.is_empty() {
            self.presets = Preset::defaults();
        }
        self.presets.iter_mut().for_each(Preset::normalise);
        let max = self.presets.iter().map(|p| p.id).max().unwrap_or(0);
        self.next_preset_id = self.next_preset_id.max(max + 1);
        let ids: Vec<PresetId> = self.presets.iter().map(|p| p.id).collect();
        self.favorites.retain(|id| ids.contains(id));
        self.favorites.dedup();
        if !ids.contains(&self.active) {
            self.active = self.favorites.first().copied().unwrap_or(ids[0]);
        }
        if let Secondary::Preset(id) = self.secondary
            && !ids.contains(&id)
        {
            self.secondary = self.first_eraser();
        }
        self.color_slots = self.color_slots.clamp(1, MAX_COLOR_SLOTS);
        self.size_slots = self.size_slots.clamp(1, MAX_SIZE_SLOTS);
        self.recent_colors.truncate(MAX_RECENT_COLORS);
    }

    /// Puts `color`, made opaque, first among the recent colours.
    pub fn add_recent_color(&mut self, color: Rgba8) {
        let color = Rgba8 { a: 255, ..color };
        self.recent_colors.retain(|&c| c != color);
        self.recent_colors.insert(0, color);
        self.recent_colors.truncate(MAX_RECENT_COLORS);
    }

    /// The first eraser preset, else the lasso.
    pub fn first_eraser(&self) -> Secondary {
        self.presets
            .iter()
            .find(|p| p.role.is_eraser())
            .map_or(Secondary::Lasso, |p| Secondary::Preset(p.id))
    }

    pub fn preset(&self, id: PresetId) -> Option<&Preset> {
        self.presets.iter().find(|p| p.id == id)
    }

    pub fn preset_mut(&mut self, id: PresetId) -> Option<&mut Preset> {
        self.presets.iter_mut().find(|p| p.id == id)
    }

    /// Adds a copy of `preset` with a new id after the original (or at the end) and returns
    /// the id.
    pub fn add_preset(&mut self, mut preset: Preset, after: Option<PresetId>) -> PresetId {
        let id = self.next_preset_id;
        self.next_preset_id += 1;
        preset.id = id;
        let at = after
            .and_then(|a| self.presets.iter().position(|p| p.id == a))
            .map_or(self.presets.len(), |i| i + 1);
        self.presets.insert(at, preset);
        id
    }

    /// Removes a preset, unless it is the last one.
    pub fn remove_preset(&mut self, id: PresetId) {
        if self.presets.len() > 1 {
            self.presets.retain(|p| p.id != id);
            self.normalise();
        }
    }

    /// Puts back the three default presets in place of every preset.
    pub fn restore_default_presets(&mut self) {
        let defaults = Settings::default();
        self.presets = defaults.presets;
        self.favorites = defaults.favorites;
        self.active = defaults.active;
        self.secondary = defaults.secondary;
        self.next_preset_id = defaults.next_preset_id;
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
                let path = vault::config_dir().join("settings.json");
                match std::fs::read_to_string(&path) {
                    Ok(json) => Settings::parse(&json).unwrap_or_else(|e| {
                        tracing::warn!("cannot read {}: {e}", path.display());
                        Settings::default()
                    }),
                    Err(_) => Settings::default(),
                }
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
    SETTINGS.with(|s| *s.borrow_mut() = Some(settings));
    save();
}

/// Changes the settings now and writes them half a second after the last such change, for
/// values that change many times a second (a slider being dragged).
pub fn change(f: impl FnOnce(&mut Settings)) {
    get();
    SETTINGS.with(|s| f(s.borrow_mut().as_mut().expect("loaded by get")));
    thread_local! {
        static PENDING: RefCell<Option<glib::SourceId>> = const { RefCell::new(None) };
    }
    PENDING.with_borrow_mut(|pending| {
        if let Some(id) = pending.take() {
            id.remove();
        }
        let id = glib::timeout_add_local_once(std::time::Duration::from_millis(500), || {
            PENDING.with_borrow_mut(|p| p.take());
            save();
        });
        *pending = Some(id);
    });
}

/// Writes the settings out.
pub fn save() {
    let settings = get();
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
    use crate::preset::{HIGHLIGHTER_ALPHA, HIGHLIGHTER_COLORS, Role};

    #[test]
    fn version_1_tools_become_presets() {
        let s = Settings::parse(
            r#"{"version":1,"pen_colors":[{"r":1,"g":2,"b":3,"a":255},
                {"r":4,"g":5,"b":6,"a":255},{"r":7,"g":8,"b":9,"a":255}],"pen_width":2.0,
                "highlighter_colors":[{"r":10,"g":20,"b":30,"a":100}],"pressure":false,
                "eraser_width":20.0,"color_slots":4}"#,
        )
        .unwrap();
        assert_eq!(s.version, VERSION);
        let [pen, highlighter, eraser] = &s.presets[..] else {
            panic!("{:?}", s.presets)
        };
        assert_eq!(pen.colors.len(), MAX_COLOR_SLOTS);
        assert_eq!(pen.colors[0], Rgba8::new(1, 2, 3, 255));
        assert_eq!(pen.colors[3], PEN_COLORS[3]);
        assert_eq!(pen.color, pen.colors[0]);
        assert_eq!((pen.width, pen.pressure), (2.0, false));
        assert_eq!(highlighter.colors[0], Rgba8::new(10, 20, 30, 255));
        assert_eq!(highlighter.colors[1], HIGHLIGHTER_COLORS[1]);
        assert_eq!(highlighter.ink_color().a, 100);
        assert_eq!(highlighter.widths, HIGHLIGHTER_WIDTHS);
        assert_eq!(eraser.width, 20.0);
        assert!(eraser.role.is_eraser());
        assert_eq!(s.favorites, [0, 1, 2]);
        assert_eq!(s.secondary, Secondary::Preset(2));
        assert_eq!((s.color_slots, s.size_slots), (4, 3));
        assert_eq!(s.select_shape, SelectShape::Lasso);
    }

    #[test]
    fn version_1_defaults_keep_the_highlighter_alpha() {
        let s = Settings::parse(r#"{"version":1}"#).unwrap();
        assert_eq!(s.presets[1].ink_color().a, HIGHLIGHTER_ALPHA);
        assert_eq!(s.presets, Settings::default().presets);
    }

    #[test]
    fn settings_round_trip() {
        let mut s = Settings {
            select_shape: SelectShape::Space,
            toolbar_position: ToolbarPosition::Left,
            secondary: Secondary::Lasso,
            ..Settings::default()
        };
        let id = s.add_preset(Preset::new(0, "Square", Role::Ink), Some(0));
        s.favorites.push(id);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""select_shape":"space""#), "{json}");
        assert!(json.contains(r#""toolbar_position":"left""#), "{json}");
        assert_eq!(Settings::parse(&json).unwrap(), s);
        assert_eq!(s.presets[1].id, 3, "inserted after the pen with a new id");
    }

    #[test]
    fn dangling_ids_are_repaired() {
        let mut s = Settings {
            color_slots: 0,
            size_slots: 99,
            ..Settings::default()
        };
        s.remove_preset(2);
        s.remove_preset(0);
        assert_eq!(s.favorites, [1]);
        assert_eq!(s.active, 1);
        assert_eq!(s.secondary, Secondary::Lasso, "no eraser left");
        assert_eq!((s.color_slots, s.size_slots), (1, MAX_SIZE_SLOTS));
        s.remove_preset(1);
        assert_eq!(s.presets.len(), 1, "the last preset stays");
        assert_eq!(s.next_preset_id, 3, "ids are never reused");
    }

    #[test]
    fn recent_colors_are_opaque_unique_and_few() {
        let mut s = Settings::default();
        for i in 0..10 {
            s.add_recent_color(Rgba8::new(i, 0, 0, 100));
        }
        s.add_recent_color(Rgba8::new(7, 0, 0, 255));
        let reds: Vec<u8> = s.recent_colors.iter().map(|c| c.r).collect();
        assert_eq!(reds, [7, 9, 8, 6, 5, 4]);
        assert!(s.recent_colors.iter().all(|c| c.a == 255));
    }
}
