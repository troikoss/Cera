// SPDX-License-Identifier: GPL-3.0-or-later
//! The colour picker in the swatch and Change style popovers: the palette and the recent
//! colours as rows of swatches with the current colour ringed, and the colour wheel above a
//! preview and a hex entry. The wheel and the entry pick without closing the popover. A
//! colour from either that isn't in the palette becomes a recent colour when the popover
//! closes, so dragging the wheel about adds one, not dozens.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use cera_core::Rgba8;
use gtk::glib;
use gtk::prelude::*;

use super::rgba;
use crate::settings;
use crate::swatch::Swatch;
use crate::wheel::ColorWheel;

/// Swatches per row; recent colours fill one row.
const COLUMNS: usize = 6;
/// A swatch button, and the dot inside it, logical px.
const BUTTON: i32 = 40;
const DOT: i32 = 28;

/// The two halves of a picker, placed by the popover: the swatches, and the wheel with the
/// hex entry.
pub(super) struct ColorPicker {
    pub swatches: gtk::Box,
    pub custom: gtk::Box,
}

/// Shared by the picker's handlers, which own it; it holds its widgets weakly so that no
/// cycle keeps a closed popover alive.
struct State {
    palette: Vec<Rgba8>,
    /// Each swatch button and its colour, for the ring.
    buttons: RefCell<Vec<(glib::WeakRef<gtk::Button>, Rgba8)>>,
    /// The last colour from the wheel or the entry, kept as recent on close.
    custom: Cell<Option<Rgba8>>,
    preview: glib::WeakRef<Swatch>,
    entry: glib::WeakRef<gtk::Entry>,
    wheel: glib::WeakRef<ColorWheel>,
    pick: Box<dyn Fn(Rgba8)>,
    /// While the wheel is dragged.
    moving: Box<dyn Fn(Rgba8)>,
}

impl State {
    /// Rings the swatch of `color` and shows it in the preview and the entry.
    fn show(&self, color: Rgba8) {
        let color = opaque(color);
        for (button, c) in self.buttons.borrow().iter() {
            let Some(button) = button.upgrade() else {
                continue;
            };
            if *c == color {
                button.add_css_class("selected");
            } else {
                button.remove_css_class("selected");
            }
        }
        if let Some(preview) = self.preview.upgrade() {
            preview.set_color(rgba(color));
        }
        if let Some(entry) = self.entry.upgrade() {
            entry.set_text(&hex(color));
            entry.remove_css_class("error");
        }
    }

    /// Shows `color` and moves the wheel's markers to it.
    fn select(&self, color: Rgba8) {
        if let Some(wheel) = self.wheel.upgrade() {
            wheel.set_color(color);
        }
        self.show(color);
    }
}

/// A picker for `current` out of `palette`. `pick` takes each chosen colour; `moving` follows
/// the wheel while it is dragged. A palette or recent swatch closes the popover when `close`
/// is set.
pub(super) fn color_picker(
    palette: &[Rgba8],
    current: Rgba8,
    close: bool,
    pick: impl Fn(Rgba8) + 'static,
    moving: impl Fn(Rgba8) + 'static,
) -> ColorPicker {
    let current = opaque(current);
    let preview = Swatch::new(rgba(current));
    let entry = gtk::Entry::builder()
        .max_length(7)
        .width_chars(8)
        .placeholder_text("#rrggbb")
        .tooltip_text("Hex colour, such as #1c71d8")
        .css_classes(["monospace"])
        .build();
    let wheel = ColorWheel::new(current);
    let state = Rc::new(State {
        palette: palette.to_vec(),
        buttons: RefCell::default(),
        custom: Cell::new(None),
        preview: preview.downgrade(),
        entry: entry.downgrade(),
        wheel: wheel.downgrade(),
        pick: Box::new(pick),
        moving: Box::new(moving),
    });

    let swatches = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    swatches.append(&heading("Palette"));
    swatches.append(&swatch_grid(&state, palette, close, false));
    let recent: Vec<Rgba8> = settings::with(|s| s.recent_colors.clone());
    if !recent.is_empty() {
        swatches.append(&heading("Recent"));
        swatches.append(&swatch_grid(&state, &recent, close, true));
    }

    let custom = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    custom.append(&heading("Custom"));
    custom.append(&wheel);
    let row = gtk::Box::builder()
        .spacing(8)
        .halign(gtk::Align::Center)
        .build();
    row.append(&preview);
    row.append(&entry);
    custom.append(&row);
    state.show(current);

    // While dragging only the preview follows; the colour is taken when the finger lifts.
    wheel.connect_changed(glib::clone!(
        #[strong]
        state,
        move |w| {
            state.show(w.color());
            (state.moving)(w.color());
        }
    ));
    wheel.connect_picked(glib::clone!(
        #[strong]
        state,
        move |w| {
            state.custom.set(Some(w.color()));
            (state.pick)(w.color());
        }
    ));
    entry.connect_activate(glib::clone!(
        #[strong]
        state,
        move |e| match parse_hex(&e.text()) {
            Some(color) => {
                state.select(color);
                state.custom.set(Some(color));
                (state.pick)(color);
            }
            None => e.add_css_class("error"),
        }
    ));
    // Closing the popover unmaps the picker.
    custom.connect_unmap(glib::clone!(
        #[strong]
        state,
        move |_| {
            if let Some(color) = state.custom.take()
                && !state.palette.contains(&color)
            {
                settings::update(|s| s.add_recent_color(color));
            }
        }
    ));
    ColorPicker { swatches, custom }
}

/// Rows of swatch buttons for `colors`. A recent colour moves to the front when picked.
fn swatch_grid(state: &Rc<State>, colors: &[Rgba8], close: bool, recent: bool) -> gtk::Grid {
    let grid = gtk::Grid::builder()
        .column_spacing(4)
        .row_spacing(4)
        .halign(gtk::Align::Start)
        .build();
    for (i, &color) in colors.iter().enumerate() {
        let color = opaque(color);
        let dot = Swatch::new(rgba(color));
        dot.set_size(DOT);
        let button = gtk::Button::builder()
            .child(&dot)
            .css_classes(["flat", "circular", "picker-swatch"])
            .width_request(BUTTON)
            .height_request(BUTTON)
            .tooltip_text(hex(color))
            .build();
        button.connect_clicked(glib::clone!(
            #[strong]
            state,
            move |b| {
                state.select(color);
                state.custom.set(None);
                if recent {
                    settings::update(|s| s.add_recent_color(color));
                }
                (state.pick)(color);
                if close && let Some(popover) = b.ancestor(gtk::Popover::static_type()) {
                    popover.downcast_ref::<gtk::Popover>().unwrap().popdown();
                }
            }
        ));
        let (col, row) = (i % COLUMNS, i / COLUMNS);
        grid.attach(&button, col as i32, row as i32, 1, 1);
        state.buttons.borrow_mut().push((button.downgrade(), color));
    }
    grid
}

fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(["caption-heading", "dim-label"])
        .build()
}

fn opaque(color: Rgba8) -> Rgba8 {
    Rgba8 { a: 255, ..color }
}

/// "#1c71d8".
fn hex(c: Rgba8) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

/// "#1c71d8", "1C71D8" or "#17d" to an opaque colour.
fn parse_hex(text: &str) -> Option<Rgba8> {
    let digits = text.trim().trim_start_matches('#');
    if !digits.is_ascii() {
        return None;
    }
    let channel = |s: &str| u8::from_str_radix(s, 16).ok();
    match digits.len() {
        6 => Some(Rgba8::new(
            channel(&digits[0..2])?,
            channel(&digits[2..4])?,
            channel(&digits[4..6])?,
            255,
        )),
        3 => {
            let short = |i: usize| channel(&digits[i..=i]).map(|v| v * 17);
            Some(Rgba8::new(short(0)?, short(1)?, short(2)?, 255))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let c = Rgba8::new(28, 113, 216, 255);
        assert_eq!(hex(c), "#1c71d8");
        assert_eq!(parse_hex("#1c71d8"), Some(c));
        assert_eq!(parse_hex(" 1C71D8 "), Some(c));
        assert_eq!(parse_hex("#17d"), Some(Rgba8::new(17, 119, 221, 255)));
    }

    #[test]
    fn bad_hex_is_rejected() {
        for text in ["", "#", "#12345", "#1234567", "#gg0000", "#ééé"] {
            assert_eq!(parse_hex(text), None, "{text}");
        }
    }
}
