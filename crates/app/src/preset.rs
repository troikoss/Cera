// SPDX-License-Identifier: GPL-3.0-or-later
//! Tool presets (docs/tool-presets.md): user-made pen-like tools. A preset is ink, a
//! highlighter or an eraser, with its own colour swatches, size slots and drawing options.
//! The app ships three (pen, highlighter, eraser); any of them can be renamed, changed,
//! duplicated or deleted. Presets live in the app settings.

use cera_core::{Layer, Pattern, Rgba8, StrokeStyle};
use serde::{Deserialize, Serialize};

/// Stable identity of a preset; survives renames and reordering.
pub type PresetId = u32;

/// Most colour swatches and size slots the tool bar shows (a preference, 3 by default).
pub const MAX_COLOR_SLOTS: usize = 6;
pub const MAX_SIZE_SLOTS: usize = 5;
/// Default full-pressure widths of the size slots, in points; the first three are shown.
pub const PEN_WIDTHS: [f32; MAX_SIZE_SLOTS] = [0.8, 1.6, 3.2, 5.0, 8.0];
pub const HIGHLIGHTER_WIDTHS: [f32; MAX_SIZE_SLOTS] = [8.0, 12.0, 20.0, 28.0, 36.0];
/// Default eraser radii of the size slots, in logical pixels (the eraser works on screen,
/// whatever the zoom).
pub const ERASER_WIDTHS: [f32; MAX_SIZE_SLOTS] = [5.0, 10.0, 20.0, 32.0, 48.0];
/// What a size slot can be set to: points for ink and highlighters, logical pixels of
/// radius for erasers.
pub const PEN_WIDTH_RANGE: (f32, f32) = (0.2, 10.0);
pub const HIGHLIGHTER_WIDTH_RANGE: (f32, f32) = (2.0, 40.0);
pub const ERASER_WIDTH_RANGE: (f32, f32) = (3.0, 60.0);

pub const PEN_COLORS: [Rgba8; MAX_COLOR_SLOTS] = [
    Rgba8::new(30, 30, 30, 255),
    Rgba8::new(28, 113, 216, 255),
    Rgba8::new(192, 28, 40, 255),
    Rgba8::new(38, 162, 105, 255),
    Rgba8::new(230, 97, 0, 255),
    Rgba8::new(129, 61, 156, 255),
];
/// The highlighter's default opacity, as alpha.
pub const HIGHLIGHTER_ALPHA: u8 = 90;
/// Highlighter tints; they are opaque here and drawn at the preset's opacity.
pub const HIGHLIGHTER_COLORS: [Rgba8; MAX_COLOR_SLOTS] = [
    Rgba8::new(246, 211, 45, 255),
    Rgba8::new(87, 227, 137, 255),
    Rgba8::new(246, 97, 81, 255),
    Rgba8::new(98, 160, 234, 255),
    Rgba8::new(220, 138, 221, 255),
    Rgba8::new(255, 163, 72, 255),
];

/// What an eraser removes. Partial erasing comes in M9.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EraserMode {
    /// Whole strokes the eraser touches.
    #[default]
    Stroke,
    /// The parts of strokes inside the eraser.
    Partial,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", from = "RoleWire")]
pub enum Role {
    Ink,
    /// Drawn over all ink.
    Highlighter,
    Eraser {
        mode: EraserMode,
    },
}

/// How a role is read: as written now, or as the first presets build wrote a highlighter
/// (`{"highlighter": {"over_ink": …}}`, before highlighters always drew over the ink).
#[derive(Deserialize)]
#[serde(untagged)]
enum RoleWire {
    Current(RoleCurrent),
    Old(RoleOld),
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum RoleCurrent {
    Ink,
    Highlighter,
    Eraser { mode: EraserMode },
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum RoleOld {
    Highlighter {
        #[serde(rename = "over_ink")]
        _over_ink: bool,
    },
}

impl From<RoleWire> for Role {
    fn from(w: RoleWire) -> Self {
        match w {
            RoleWire::Current(RoleCurrent::Ink) => Role::Ink,
            RoleWire::Current(RoleCurrent::Highlighter)
            | RoleWire::Old(RoleOld::Highlighter { .. }) => Role::Highlighter,
            RoleWire::Current(RoleCurrent::Eraser { mode }) => Role::Eraser { mode },
        }
    }
}

impl Role {
    pub fn is_eraser(self) -> bool {
        matches!(self, Role::Eraser { .. })
    }

    pub fn icon(self) -> &'static str {
        match self {
            Role::Ink => "cera-pen-symbolic",
            Role::Highlighter => "cera-highlighter-symbolic",
            Role::Eraser { .. } => "cera-eraser-symbolic",
        }
    }

    pub fn width_range(self) -> (f32, f32) {
        match self {
            Role::Ink => PEN_WIDTH_RANGE,
            Role::Highlighter => HIGHLIGHTER_WIDTH_RANGE,
            Role::Eraser { .. } => ERASER_WIDTH_RANGE,
        }
    }

    pub fn default_widths(self) -> [f32; MAX_SIZE_SLOTS] {
        match self {
            Role::Ink => PEN_WIDTHS,
            Role::Highlighter => HIGHLIGHTER_WIDTHS,
            Role::Eraser { .. } => ERASER_WIDTHS,
        }
    }
}

/// A preset's line pattern; drawn from M9.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinePattern {
    #[default]
    Solid,
    Dash,
    Dot,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preset {
    pub id: PresetId,
    pub name: String,
    pub role: Role,
    /// Swatch colours, opaque, `MAX_COLOR_SLOTS` of them; the first `color_slots` are shown.
    pub colors: Vec<Rgba8>,
    /// Size slot widths, `MAX_SIZE_SLOTS` of them; the first `size_slots` are shown.
    pub widths: Vec<f32>,
    /// The current colour (opaque) and width.
    pub color: Rgba8,
    pub width: f32,
    /// `0.0..=1.0`; a stroke never darkens itself, separate strokes stack.
    pub opacity: f32,
    /// The width follows the stylus pressure; off, every sample counts as full pressure.
    pub pressure: bool,
    /// The nib: 0 a square aligned to the page, 1 a disc.
    pub roundness: f32,
    pub pattern: LinePattern,
    /// A dashed or dotted line's gap (and dash length) in multiples of the width.
    pub spacing: f32,
}

impl Default for Preset {
    fn default() -> Self {
        Self::new(0, "Pen", Role::Ink)
    }
}

impl Preset {
    /// A preset with the defaults for its role.
    pub fn new(id: PresetId, name: &str, role: Role) -> Self {
        let highlighter = matches!(role, Role::Highlighter);
        let colors = if highlighter {
            HIGHLIGHTER_COLORS
        } else {
            PEN_COLORS
        };
        let widths = role.default_widths();
        Self {
            id,
            name: name.to_owned(),
            role,
            colors: colors.to_vec(),
            widths: widths.to_vec(),
            color: colors[0],
            width: widths[1],
            opacity: if highlighter {
                f32::from(HIGHLIGHTER_ALPHA) / 255.0
            } else {
                1.0
            },
            pressure: role == Role::Ink,
            // Highlighters have a square tip, like a felt marker.
            roundness: if highlighter { 0.0 } else { 1.0 },
            pattern: LinePattern::Solid,
            spacing: 1.0,
        }
    }

    /// The presets the app ships with: pen, highlighter, eraser (ids 0, 1, 2).
    pub fn defaults() -> Vec<Preset> {
        vec![
            Preset::new(0, "Pen", Role::Ink),
            Preset::new(1, "Highlighter", Role::Highlighter),
            Preset::new(
                2,
                "Eraser",
                Role::Eraser {
                    mode: EraserMode::Stroke,
                },
            ),
        ]
    }

    /// The colour strokes are drawn in: the current colour at the preset's opacity.
    pub fn ink_color(&self) -> Rgba8 {
        Rgba8 {
            a: (self.opacity.clamp(0.0, 1.0) * 255.0).round() as u8,
            ..self.color
        }
    }

    pub fn stroke_style(&self) -> StrokeStyle {
        StrokeStyle {
            layer: match self.role {
                Role::Highlighter => Layer::Over,
                Role::Ink | Role::Eraser { .. } => Layer::Ink,
            },
            roundness: (self.roundness.clamp(0.0, 1.0) * 255.0).round() as u8,
            pattern: match self.pattern {
                LinePattern::Solid => Pattern::Solid,
                LinePattern::Dash => Pattern::Dash,
                LinePattern::Dot => Pattern::Dot,
            },
            spacing: (self.spacing * 16.0).round().clamp(1.0, 255.0) as u8,
        }
    }

    /// Changes the role; switching to or from an eraser resets the size slots, whose units
    /// differ, and widths are kept inside the new role's range.
    pub fn set_role(&mut self, role: Role) {
        if role.is_eraser() != self.role.is_eraser() {
            self.widths = role.default_widths().to_vec();
            self.width = self.widths[1];
        }
        self.role = role;
        let (min, max) = role.width_range();
        for w in self.widths.iter_mut().chain([&mut self.width]) {
            *w = w.clamp(min, max);
        }
    }

    /// Pads the slot lists to their full length and keeps values in range.
    pub fn normalise(&mut self) {
        let defaults = Preset::new(self.id, &self.name, self.role);
        self.colors.truncate(MAX_COLOR_SLOTS);
        self.colors
            .extend_from_slice(&defaults.colors[self.colors.len()..]);
        self.widths.truncate(MAX_SIZE_SLOTS);
        self.widths
            .extend_from_slice(&defaults.widths[self.widths.len()..]);
        self.opacity = self.opacity.clamp(0.05, 1.0);
        self.roundness = self.roundness.clamp(0.0, 1.0);
        self.spacing = self.spacing.clamp(0.25, 8.0);
        let (min, max) = self.role.width_range();
        for w in self.widths.iter_mut().chain([&mut self.width]) {
            *w = w.clamp(min, max);
        }
    }
}

/// What the stylus's eraser end and barrel button do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Secondary {
    Preset(PresetId),
    Lasso,
}

impl Default for Secondary {
    fn default() -> Self {
        Secondary::Preset(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlighter_draws_translucent_over_the_ink() {
        let presets = Preset::defaults();
        let h = &presets[1];
        assert_eq!(
            h.ink_color(),
            Rgba8 {
                a: 90,
                ..HIGHLIGHTER_COLORS[0]
            }
        );
        let style = h.stroke_style();
        assert_eq!((style.layer, style.roundness), (Layer::Over, 0));
        assert!(!h.pressure);
        assert_eq!(presets[0].stroke_style(), StrokeStyle::PEN);
    }

    #[test]
    fn becoming_an_eraser_resets_the_sizes() {
        let mut p = Preset::new(4, "Marker", Role::Ink);
        p.width = 9.0;
        p.set_role(Role::Highlighter);
        assert_eq!(p.width, 9.0, "inside the highlighter's range");
        assert_eq!(p.widths[0], HIGHLIGHTER_WIDTH_RANGE.0, "clamped");
        p.set_role(Role::Eraser {
            mode: EraserMode::Stroke,
        });
        assert_eq!(p.widths, ERASER_WIDTHS);
        assert_eq!(p.width, ERASER_WIDTHS[1]);
    }

    #[test]
    fn roles_read_old_and_new() {
        let read = |json: &str| serde_json::from_str::<Role>(json).unwrap();
        assert_eq!(read(r#""highlighter""#), Role::Highlighter);
        assert_eq!(
            read(r#"{"highlighter":{"over_ink":false}}"#),
            Role::Highlighter
        );
        assert_eq!(read(r#""ink""#), Role::Ink);
        let eraser = Role::Eraser {
            mode: EraserMode::Stroke,
        };
        assert_eq!(read(r#"{"eraser":{"mode":"stroke"}}"#), eraser);
        for role in [Role::Ink, Role::Highlighter, eraser] {
            assert_eq!(read(&serde_json::to_string(&role).unwrap()), role);
        }
    }
}
