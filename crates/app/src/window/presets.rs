// SPDX-License-Identifier: GPL-3.0-or-later
//! Preset options (docs/tool-presets.md): the editor shown when the active preset is tapped
//! in the tool bar, and the Tools page in Preferences that lists every preset (favourite,
//! edit, duplicate, delete, add, restore the defaults) and picks the stylus's secondary
//! action. Options an eraser doesn't use are greyed out.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use super::CeraWindow;
use super::prefs::for_each_window;
use super::toolbar::popover_box;
use crate::preset::{EraserMode, Preset, PresetId, Role, Secondary};
use crate::settings;

/// The kinds in the editor's drop-down, in order.
const KINDS: [&str; 3] = ["Ink", "Highlighter", "Eraser"];

fn kind_index(role: Role) -> u32 {
    match role {
        Role::Ink => 0,
        Role::Highlighter => 1,
        Role::Eraser { .. } => 2,
    }
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::Ink => "Ink",
        Role::Highlighter => "Highlighter",
        Role::Eraser { .. } => "Eraser",
    }
}

/// Changes preset `id` and writes the settings; `refresh` also rebuilds every tool bar
/// (for a new name, kind or colour).
fn edit(id: PresetId, refresh: bool, f: impl FnOnce(&mut Preset)) {
    settings::update(|s| {
        if let Some(p) = s.preset_mut(id) {
            f(p);
        }
    });
    if refresh {
        for_each_window(CeraWindow::rebuild_tools);
    }
}

/// A row with a slider, for a value in `0.0..=1.0` shown as a percentage. The settings are
/// written once the slider rests.
fn slider_row(
    title: &str,
    value: f32,
    min: f64,
    marks: Option<(&str, &str)>,
    set: impl Fn(f32) + 'static,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, 1.0, 0.05);
    scale.set_value(f64::from(value));
    scale.set_width_request(180);
    scale.set_round_digits(2);
    scale.set_valign(gtk::Align::Center);
    if let Some((low, high)) = marks {
        scale.add_mark(min, gtk::PositionType::Bottom, Some(low));
        scale.add_mark(1.0, gtk::PositionType::Bottom, Some(high));
    } else {
        scale.set_draw_value(true);
        scale.set_value_pos(gtk::PositionType::Left);
        scale.set_format_value_func(|_, v| format!("{:.0} %", v * 100.0));
    }
    scale.connect_value_changed(move |s| set(s.value() as f32));
    row.add_suffix(&scale);
    row
}

/// The options of preset `id`, as preference groups in a box.
pub(super) fn preset_editor(id: PresetId) -> gtk::Box {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();
    let Some(preset) = settings::with(|s| s.preset(id).cloned()) else {
        return content;
    };

    let general = adw::PreferencesGroup::new();
    let name = adw::EntryRow::builder()
        .title("Name")
        .text(&preset.name)
        .show_apply_button(true)
        .build();
    name.connect_apply(move |row| {
        let text = row.text().trim().to_owned();
        if !text.is_empty() {
            edit(id, true, |p| p.name = text);
        }
    });
    let kind = adw::ComboRow::builder()
        .title("Kind")
        .model(&gtk::StringList::new(&KINDS))
        .selected(kind_index(preset.role))
        .build();
    general.add(&name);
    general.add(&kind);

    let stroke = adw::PreferencesGroup::builder().title("Stroke").build();
    let stroke_group = stroke.clone();
    let pressure = adw::SwitchRow::builder()
        .title("Pressure")
        .subtitle("The width follows how hard you press")
        .active(preset.pressure)
        .build();
    pressure.connect_active_notify(move |row| {
        let on = row.is_active();
        edit(id, false, |p| p.pressure = on);
    });
    let opacity = slider_row("Opacity", preset.opacity, 0.05, None, move |v| {
        settings::change(|s| {
            if let Some(p) = s.preset_mut(id) {
                p.opacity = v;
            }
        });
    });
    let tip = slider_row(
        "Tip",
        preset.roundness,
        0.0,
        Some(("Square", "Round")),
        move |v| {
            settings::change(|s| {
                if let Some(p) = s.preset_mut(id) {
                    p.roundness = v;
                }
            });
        },
    );
    stroke.add(&pressure);
    stroke.add(&opacity);
    stroke.add(&tip);

    // An eraser uses none of these; ink is always opaque.
    let sensitive = move |role: Role| {
        stroke.set_sensitive(!role.is_eraser());
        opacity.set_sensitive(role == Role::Highlighter);
    };
    sensitive(preset.role);
    kind.connect_selected_notify(glib::clone!(
        #[strong]
        sensitive,
        move |row| {
            let role = match row.selected() {
                0 => Role::Ink,
                1 => Role::Highlighter,
                _ => Role::Eraser {
                    mode: EraserMode::Stroke,
                },
            };
            sensitive(role);
            edit(id, true, |p| p.set_role(role));
        }
    ));

    content.append(&general);
    content.append(&stroke_group);
    content
}

impl CeraWindow {
    /// The tool bar popover for the active preset: its options and a way to all presets.
    pub(super) fn preset_popover(&self, id: PresetId) -> gtk::Box {
        let content = popover_box(gtk::Orientation::Vertical);
        content.set_width_request(360);
        content.append(&preset_editor(id));
        let manage = gtk::Button::builder()
            .label("_Manage Presets…")
            .use_underline(true)
            .halign(gtk::Align::Center)
            .css_classes(["pill"])
            .build();
        manage.connect_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |b| {
                if let Some(popover) = b
                    .ancestor(gtk::Popover::static_type())
                    .and_downcast::<gtk::Popover>()
                {
                    popover.popdown();
                }
                win.show_preferences_page(Some("tools"));
            }
        ));
        // The popover focuses its first field, the name, which would bring up the on-screen
        // keyboard; start on the button instead.
        manage.connect_map(|b| {
            let b = b.clone();
            glib::idle_add_local_once(move || {
                b.grab_focus();
            });
        });
        content.append(&manage);
        content
    }
}

/// The Tools page of the preferences dialog. Its groups are rebuilt whenever a preset is
/// added, removed or edited.
pub(super) struct ToolsPage {
    pub page: adw::PreferencesPage,
    dialog: adw::PreferencesDialog,
    groups: RefCell<Vec<adw::PreferencesGroup>>,
}

impl ToolsPage {
    pub fn new(dialog: &adw::PreferencesDialog) -> Rc<Self> {
        let page = adw::PreferencesPage::builder()
            .name("tools")
            .title("Tools")
            .icon_name("cera-pen-symbolic")
            .build();
        let this = Rc::new(Self {
            page,
            dialog: dialog.clone(),
            groups: RefCell::default(),
        });
        this.fill();
        this
    }

    fn fill(self: &Rc<Self>) {
        for group in self.groups.take() {
            self.page.remove(&group);
        }
        let current = settings::get();

        let list = adw::PreferencesGroup::builder()
            .title("Presets")
            .description("Starred presets are in the tool bar, in the order they were starred")
            .build();
        let add = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add preset")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        add.connect_clicked(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| {
                let mut id = 0;
                settings::update(|s| {
                    id = s.add_preset(Preset::new(0, "New pen", Role::Ink), None);
                    s.favorites.push(id);
                });
                for_each_window(CeraWindow::rebuild_tools);
                this.fill();
                this.open_editor(id);
            }
        ));
        list.set_header_suffix(Some(&add));
        for preset in &current.presets {
            list.add(&self.preset_row(preset, current.favorites.contains(&preset.id)));
        }

        let stylus = adw::PreferencesGroup::builder().title("Stylus").build();
        let mut names: Vec<String> = current.presets.iter().map(|p| p.name.clone()).collect();
        names.push("Lasso".to_owned());
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let selected = match current.secondary {
            Secondary::Preset(id) => current.presets.iter().position(|p| p.id == id),
            Secondary::Lasso => None,
        }
        .unwrap_or(current.presets.len());
        let secondary = adw::ComboRow::builder()
            .title("Eraser end and button")
            .subtitle("What the stylus does with its eraser end or with its button held")
            .model(&gtk::StringList::new(&names))
            .selected(selected as u32)
            .build();
        let ids: Vec<PresetId> = current.presets.iter().map(|p| p.id).collect();
        secondary.connect_selected_notify(move |row| {
            let choice = ids
                .get(row.selected() as usize)
                .map_or(Secondary::Lasso, |&id| Secondary::Preset(id));
            settings::update(|s| s.secondary = choice);
        });
        stylus.add(&secondary);

        let restore_group = adw::PreferencesGroup::new();
        let restore = adw::ButtonRow::builder()
            .title("_Restore Default Presets")
            .use_underline(true)
            .css_classes(["destructive-action"])
            .build();
        restore.connect_activated(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.confirm_restore()
        ));
        restore_group.add(&restore);

        for group in [list, stylus, restore_group] {
            self.page.add(&group);
            self.groups.borrow_mut().push(group);
        }
    }

    /// A preset's row: its icon, name and kind, a favourite star and a menu.
    fn preset_row(self: &Rc<Self>, preset: &Preset, favorite: bool) -> adw::ActionRow {
        let id = preset.id;
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&preset.name))
            .subtitle(role_label(preset.role))
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(preset.role.icon()));
        let star = gtk::ToggleButton::builder()
            .icon_name(if favorite {
                "starred-symbolic"
            } else {
                "non-starred-symbolic"
            })
            .active(favorite)
            .tooltip_text("In the tool bar")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        star.connect_toggled(move |b| {
            let on = b.is_active();
            b.set_icon_name(if on {
                "starred-symbolic"
            } else {
                "non-starred-symbolic"
            });
            settings::update(|s| {
                s.favorites.retain(|f| *f != id);
                if on {
                    s.favorites.push(id);
                }
            });
            for_each_window(CeraWindow::rebuild_tools);
        });
        row.add_suffix(&star);

        let menu = gio::Menu::new();
        menu.append(Some("_Edit"), Some("preset.edit"));
        menu.append(Some("_Duplicate"), Some("preset.duplicate"));
        menu.append(Some("De_lete"), Some("preset.delete"));
        let more = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .menu_model(&menu)
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        row.add_suffix(&more);
        let actions = gio::SimpleActionGroup::new();
        let action = |name: &str, f: fn(&Rc<ToolsPage>, PresetId)| {
            let a = gio::SimpleAction::new(name, None);
            a.connect_activate(glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |_, _| f(&this, id)
            ));
            actions.add_action(&a);
            a
        };
        action("edit", Self::open_editor);
        action("duplicate", |this, id| {
            let mut new = None;
            settings::update(|s| {
                if let Some(p) = s.preset(id).cloned() {
                    let copy = Preset {
                        name: format!("{} (copy)", p.name),
                        ..p
                    };
                    let added = s.add_preset(copy, Some(id));
                    s.favorites.push(added);
                    new = Some(added);
                }
            });
            for_each_window(CeraWindow::rebuild_tools);
            this.fill();
            if let Some(id) = new {
                this.open_editor(id);
            }
        });
        let delete = action("delete", |this, id| {
            settings::update(|s| s.remove_preset(id));
            for_each_window(CeraWindow::rebuild_tools);
            this.fill();
        });
        // The last preset can't be deleted.
        delete.set_enabled(settings::with(|s| s.presets.len() > 1));
        row.insert_action_group("preset", Some(&actions));
        row.connect_activated(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.open_editor(id)
        ));
        row
    }

    /// The preset's options on a subpage; the list is refreshed when it is closed.
    fn open_editor(self: &Rc<Self>, id: PresetId) {
        let Some(name) = settings::with(|s| s.preset(id).map(|p| p.name.clone())) else {
            return;
        };
        let editor = preset_editor(id);
        editor.set_margin_top(12);
        editor.set_margin_bottom(24);
        editor.set_margin_start(12);
        editor.set_margin_end(12);
        let clamp = adw::Clamp::builder().child(&editor).build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .child(&clamp)
            .build();
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&scroll));
        let page = adw::NavigationPage::builder()
            .title(name)
            .child(&view)
            .build();
        page.connect_hidden(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.fill()
        ));
        self.dialog.push_subpage(&page);
    }

    fn confirm_restore(self: &Rc<Self>) {
        let alert = adw::AlertDialog::builder()
            .heading("Restore Default Presets?")
            .body("Your presets are replaced by the pen, highlighter and eraser.")
            .default_response("cancel")
            .close_response("cancel")
            .build();
        alert.add_responses(&[("cancel", "_Cancel"), ("restore", "_Restore")]);
        alert.set_response_appearance("restore", adw::ResponseAppearance::Destructive);
        alert.connect_response(
            Some("restore"),
            glib::clone!(
                #[weak(rename_to = this)]
                self,
                move |_, _| {
                    settings::update(settings::Settings::restore_default_presets);
                    for_each_window(CeraWindow::rebuild_tools);
                    this.fill();
                }
            ),
        );
        alert.present(Some(&self.dialog));
    }
}
