// SPDX-License-Identifier: GPL-3.0-or-later
//! Preferences (paper for new notes in this vault, the tool bar's position and slots, touch gestures, tile
//! cache) and the keyboard shortcuts list.

use adw::prelude::*;
use adw::subclass::prelude::*;
use cera_core::PaperKind;
use gtk::{gio, glib};

use super::CeraWindow;
use crate::canvas::{MAX_COLOR_SLOTS, MAX_SIZE_SLOTS};
use crate::settings::{self, ToolbarPosition};
use crate::vault::{PageSize, VaultConfig};

const PAPERS: [(PaperKind, &str); 4] = [
    (PaperKind::Blank, "Blank"),
    (PaperKind::Ruled, "Ruled"),
    (PaperKind::Grid, "Grid"),
    (PaperKind::Dots, "Dots"),
];
const SIZES: [(PageSize, &str); 2] = [(PageSize::A4, "A4"), (PageSize::Letter, "Letter")];

impl CeraWindow {
    pub(super) fn show_preferences(&self) {
        let page = adw::PreferencesPage::new();
        let vault = self.imp().vault.borrow().clone();
        if let Some(vault) = vault {
            let new = vault.config.new_note.clone();
            let group = adw::PreferencesGroup::builder()
                .title("New Notes and Pages")
                .description(format!("In the vault “{}”", vault.name()))
                .build();
            let paper = adw::ComboRow::builder()
                .title("Paper")
                .model(&gtk::StringList::new(&PAPERS.map(|(_, n)| n)))
                .selected(
                    PAPERS
                        .iter()
                        .position(|(k, _)| *k == new.paper)
                        .unwrap_or(0) as u32,
                )
                .build();
            paper.connect_selected_notify(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |row| {
                    let kind = PAPERS[row.selected() as usize % PAPERS.len()].0;
                    win.update_vault_config(|c| c.new_note.paper = kind);
                }
            ));
            let size = adw::ComboRow::builder()
                .title("Page size")
                .model(&gtk::StringList::new(&SIZES.map(|(_, n)| n)))
                .selected(SIZES.iter().position(|(s, _)| *s == new.size).unwrap_or(0) as u32)
                .build();
            size.connect_selected_notify(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |row| {
                    let size = SIZES[row.selected() as usize % SIZES.len()].0;
                    win.update_vault_config(|c| c.new_note.size = size);
                }
            ));
            let landscape = adw::SwitchRow::builder()
                .title("Landscape")
                .active(new.landscape)
                .build();
            landscape.connect_active_notify(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |row| {
                    let on = row.is_active();
                    win.update_vault_config(|c| c.new_note.landscape = on);
                }
            ));
            let spacing = adw::SpinRow::builder()
                .title("Line spacing")
                .subtitle("Millimetres")
                .digits(1)
                .adjustment(&gtk::Adjustment::new(
                    f64::from(new.spacing_mm),
                    3.0,
                    20.0,
                    0.5,
                    1.0,
                    0.0,
                ))
                .build();
            spacing.connect_value_notify(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |row| {
                    let mm = row.value() as f32;
                    win.update_vault_config(|c| c.new_note.spacing_mm = mm);
                }
            ));
            group.add(&paper);
            group.add(&size);
            group.add(&landscape);
            group.add(&spacing);
            page.add(&group);
        }

        let current = settings::get();
        let toolbar = adw::PreferencesGroup::builder()
            .title("Tool Bar")
            .description("Tap the selected colour or size in the tool bar to change it")
            .build();
        let slots = |title: &str, value: usize, max: usize| {
            adw::SpinRow::builder()
                .title(title)
                .adjustment(&gtk::Adjustment::new(
                    value as f64,
                    1.0,
                    max as f64,
                    1.0,
                    1.0,
                    0.0,
                ))
                .build()
        };
        let colors = slots("Colours", current.color_slots, MAX_COLOR_SLOTS);
        colors.connect_value_notify(|row| {
            let n = row.value() as usize;
            settings::update(|s| s.color_slots = n);
            for_each_window(CeraWindow::rebuild_slots);
        });
        let sizes = slots("Sizes", current.size_slots, MAX_SIZE_SLOTS);
        sizes.connect_value_notify(|row| {
            let n = row.value() as usize;
            settings::update(|s| s.size_slots = n);
            for_each_window(CeraWindow::rebuild_slots);
        });
        let position = adw::ComboRow::builder()
            .title("Position")
            .model(&gtk::StringList::new(
                &ToolbarPosition::ALL.map(ToolbarPosition::label),
            ))
            .selected(
                ToolbarPosition::ALL
                    .iter()
                    .position(|p| *p == current.toolbar_position)
                    .unwrap_or(0) as u32,
            )
            .build();
        position.connect_selected_notify(|row| {
            let all = ToolbarPosition::ALL;
            let position = all[row.selected() as usize % all.len()];
            settings::update(|s| s.toolbar_position = position);
            for_each_window(CeraWindow::place_toolbar);
        });
        toolbar.add(&position);
        toolbar.add(&colors);
        toolbar.add(&sizes);
        page.add(&toolbar);

        let touch = adw::PreferencesGroup::builder().title("Touch").build();
        let taps = adw::SwitchRow::builder()
            .title("Undo with two fingers")
            .subtitle("Tap with two fingers to undo, with three to redo")
            .active(current.finger_taps)
            .build();
        taps.connect_active_notify(|row| {
            let on = row.is_active();
            settings::update(|s| s.finger_taps = on);
        });
        let finger = adw::SwitchRow::builder()
            .title("Draw with finger")
            .subtitle("One finger draws; two fingers scroll and zoom")
            .active(current.draw_with_finger)
            .build();
        finger.connect_active_notify(|row| {
            let on = row.is_active();
            settings::update(|s| s.draw_with_finger = on);
        });
        touch.add(&taps);
        touch.add(&finger);
        page.add(&touch);

        let memory = adw::PreferencesGroup::builder().title("Memory").build();
        let budget = adw::SpinRow::builder()
            .title("Page cache")
            .subtitle("MiB of rendered page tiles kept, per note")
            .adjustment(&gtk::Adjustment::new(
                f64::from(current.tile_budget_mib),
                32.0,
                1024.0,
                16.0,
                64.0,
                0.0,
            ))
            .build();
        budget.connect_value_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |row| {
                let mib = row.value() as u32;
                settings::update(|s| s.tile_budget_mib = mib);
                for canvas in win.canvases() {
                    canvas.set_tile_budget((mib as usize) << 20);
                }
            }
        ));
        memory.add(&budget);
        page.add(&memory);

        let dialog = adw::PreferencesDialog::new();
        dialog.add(&page);
        dialog.present(Some(self));
    }

    fn update_vault_config(&self, f: impl FnOnce(&mut VaultConfig)) {
        let saved = {
            let mut vault = self.imp().vault.borrow_mut();
            let Some(vault) = vault.as_mut() else { return };
            f(&mut vault.config);
            vault.save_config()
        };
        if let Err(e) = saved {
            self.toast(&format!("Cannot save the vault settings: {e}"));
        }
    }

    pub(super) fn show_shortcuts(&self) {
        let groups: [(&str, &[(&str, &str)]); 4] = [
            (
                "General",
                &[
                    ("New note", "<Control>n"),
                    ("Open file", "<Control>o"),
                    ("Search notes", "<Control>f"),
                    ("Close tab", "<Control>w"),
                    ("Next and previous tab", "<Control>Tab <Control><Shift>Tab"),
                    ("Toggle sidebar", "F9"),
                    ("Preferences", "<Control>comma"),
                    ("Keyboard shortcuts", "<Control>question"),
                    ("Quit", "<Control>q"),
                ],
            ),
            (
                "Note",
                &[
                    ("Undo", "<Control>z"),
                    ("Redo", "<Control><Shift>z <Control>y"),
                    ("Zoom in", "<Control>plus"),
                    ("Zoom out", "<Control>minus"),
                    ("Fit width", "<Control>0"),
                    ("Pages", "<Control>p"),
                    ("Export as PDF", "<Control>e"),
                    ("Export page as PNG", "<Control><Shift>e"),
                    ("Add page", "<Control>Return"),
                    ("Bookmark page", "<Control>d"),
                ],
            ),
            (
                "Tools",
                &[
                    ("Pen", "p"),
                    ("Highlighter", "h"),
                    ("Eraser", "e"),
                    ("Lasso", "l"),
                ],
            ),
            (
                "Selection",
                &[
                    ("Cut", "<Control>x"),
                    ("Copy", "<Control>c"),
                    ("Paste", "<Control>v"),
                    ("Delete", "Delete"),
                    ("Deselect", "Escape"),
                    ("Bring forward", "<Control>bracketright"),
                    ("Send backward", "<Control>bracketleft"),
                    ("Bring to front", "<Control><Shift>bracketright"),
                    ("Send to back", "<Control><Shift>bracketleft"),
                ],
            ),
        ];
        let page = adw::PreferencesPage::new();
        for (title, rows) in groups {
            let group = adw::PreferencesGroup::builder().title(title).build();
            for (name, accel) in rows {
                let row = adw::ActionRow::builder().title(*name).build();
                let keys: Vec<String> = accel
                    .split_whitespace()
                    .filter_map(gtk::accelerator_parse)
                    .map(|(key, mods)| gtk::accelerator_get_label(key, mods).to_string())
                    .collect();
                let label = gtk::Label::builder()
                    .label(keys.join(" or "))
                    .valign(gtk::Align::Center)
                    .css_classes(["dim-label", "numeric"])
                    .build();
                row.add_suffix(&label);
                group.add(&row);
            }
            page.add(&group);
        }
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&page));
        let dialog = adw::Dialog::builder()
            .title("Keyboard Shortcuts")
            .content_width(480)
            .content_height(640)
            .child(&view)
            .build();
        dialog.present(Some(self));
    }
}

/// Applies a tool bar preference to every window.
fn for_each_window(f: impl Fn(&CeraWindow)) {
    let Some(app) = gio::Application::default().and_downcast::<gtk::Application>() else {
        return;
    };
    for win in app.windows() {
        if let Ok(win) = win.downcast::<CeraWindow>() {
            f(&win);
        }
    }
}
