// SPDX-License-Identifier: GPL-3.0-or-later
//! The floating tool bar (DESIGN.md §9): the favourite presets and the lasso, colour swatches,
//! size slots, undo and redo (the lasso's actions are in the canvas's context menu,
//! `show_context_menu`). Which window edge it sits on and how many swatches and size slots it
//! shows are preferences; an eraser preset has size slots but no swatches, the lasso neither
//! (Change style in the context menu recolours a selection). Tapping the active preset again
//! opens its options (`presets.rs`), the active lasso its shape; tapping the selected swatch
//! again opens the palette and colour wheel for that slot, the selected size slot a slider.
//! Presets, their slots and the current colour and width are saved in the app settings.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use cera_core::{Layer, Rgba8, StrokeStyle};
use gtk::{gdk, gio, glib};

use super::colors::color_picker;
use super::{CeraWindow, rgba};
use crate::canvas::{InputSource, ToolKind};
use crate::preset::{HIGHLIGHTER_ALPHA, HIGHLIGHTER_COLORS, Preset, PresetId, Role};
use crate::settings::{self, SelectShape, ToolbarPosition};
use crate::swatch::Swatch;

/// Palette choices: GNOME's palette for ink, tints for highlighters.
const PEN_PRESETS: [Rgba8; 12] = [
    Rgba8::new(30, 30, 30, 255),
    Rgba8::new(94, 92, 100, 255),
    Rgba8::new(28, 113, 216, 255),
    Rgba8::new(98, 160, 234, 255),
    Rgba8::new(38, 162, 105, 255),
    Rgba8::new(46, 194, 126, 255),
    Rgba8::new(229, 165, 10, 255),
    Rgba8::new(230, 97, 0, 255),
    Rgba8::new(192, 28, 40, 255),
    Rgba8::new(237, 51, 59, 255),
    Rgba8::new(129, 61, 156, 255),
    Rgba8::new(134, 94, 60, 255),
];
/// Smallest and largest dot of a size slot, logical px.
const DOT_RANGE: (f32, f32) = (5.0, 22.0);
/// The colour dot on a preset's tool button.
const TOOL_DOT: i32 = 8;

impl CeraWindow {
    pub(super) fn setup_toolbar(&self) {
        let imp = self.imp();
        let tools = &imp.tool_state;
        let (active, shape) = settings::with(|s| (s.active, s.select_shape));
        tools.tool.set(ToolKind::Preset(active));
        tools.select_shape.set(shape);
        imp.tools.connect_active_name_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |group| {
                // Rebuilding the group passes through no active toggle.
                let Some(tool) = group.active_name().as_deref().and_then(ToolKind::from_name)
                else {
                    return;
                };
                let imp = win.imp();
                if imp.rebuilding_tools.get() {
                    return;
                }
                imp.tool_state.tool.set(tool);
                if let ToolKind::Preset(id) = tool {
                    settings::update(|s| s.active = id);
                }
                if tool != ToolKind::Lasso
                    && let Some(canvas) = win.canvas()
                {
                    canvas.clear_selection();
                }
                win.update_toolbar();
            }
        ));

        // Tapping the active tool again: let the toggle group switch first, then compare.
        let reclick = gtk::GestureClick::new();
        reclick.set_propagation_phase(gtk::PropagationPhase::Capture);
        let pressed_on = Rc::new(std::cell::RefCell::new(None::<glib::GString>));
        reclick.connect_pressed(glib::clone!(
            #[weak(rename_to = win)]
            self,
            #[strong]
            pressed_on,
            move |_, _, _, _| *pressed_on.borrow_mut() = win.imp().tools.active_name()
        ));
        reclick.connect_released(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_, _, x, y| {
                win.imp().tool_tap.set((x, y));
                let before = pressed_on.borrow_mut().take();
                glib::idle_add_local_once(glib::clone!(
                    #[weak]
                    win,
                    move || {
                        if win.imp().tools.active_name() == before {
                            win.show_tool_popover();
                        }
                    }
                ));
            }
        ));
        imp.tools.add_controller(reclick);
        self.rebuild_tools();
        self.place_toolbar();
        self.rebuild_slots();
    }

    /// Fills the tool group with the favourite presets and the lasso, keeping the selected
    /// tool if it is still there (else the first favourite).
    pub(super) fn rebuild_tools(&self) {
        let imp = self.imp();
        let group = &*imp.tools;
        imp.rebuilding_tools.set(true);
        group.remove_all();
        let mut dots = Vec::new();
        let favorites: Vec<Preset> = settings::with(|s| {
            s.favorites
                .iter()
                .filter_map(|id| s.preset(*id).cloned())
                .collect()
        });
        for (i, preset) in favorites.iter().enumerate() {
            let icon = gtk::Image::from_icon_name(preset.role.icon());
            let child = gtk::Overlay::builder().child(&icon).build();
            if !preset.role.is_eraser() {
                let dot = Swatch::new(rgba(preset.color));
                dot.set_size(TOOL_DOT);
                dot.set_halign(gtk::Align::End);
                dot.set_valign(gtk::Align::End);
                dot.set_can_target(false);
                child.add_overlay(&dot);
                dots.push((preset.id, dot));
            }
            let tooltip = match i {
                0..9 => format!("{} ({})", preset.name, i + 1),
                _ => preset.name.clone(),
            };
            let toggle = adw::Toggle::builder()
                .name(ToolKind::Preset(preset.id).name())
                .child(&child)
                .tooltip(tooltip)
                .build();
            group.add(toggle);
        }
        group.add(adw::Toggle::builder().name("lasso").build());
        *imp.tool_dots.borrow_mut() = dots;
        self.update_lasso_icon();
        let tool = match imp.tool_state.tool.get() {
            ToolKind::Preset(id) if favorites.iter().any(|p| p.id == id) => ToolKind::Preset(id),
            ToolKind::Lasso => ToolKind::Lasso,
            ToolKind::Preset(_) => favorites
                .first()
                .map_or(ToolKind::Lasso, |p| ToolKind::Preset(p.id)),
        };
        imp.tool_state.tool.set(tool);
        group.set_active_name(Some(&tool.name()));
        imp.rebuilding_tools.set(false);
        self.update_toolbar();
    }

    /// Selects the `n`th favourite (from 0), for the 1–9 keys.
    pub(super) fn select_favorite(&self, n: usize) {
        if let Some(id) = settings::with(|s| s.favorites.get(n).copied()) {
            self.imp()
                .tools
                .set_active_name(Some(&ToolKind::Preset(id).name()));
        }
    }

    /// Puts the tool bar on the window edge the preferences ask for, laid out along it.
    pub(super) fn place_toolbar(&self) {
        let imp = self.imp();
        let position = settings::with(|s| s.toolbar_position);
        let vertical = position.is_vertical();
        let scroll = &*imp.toolbar_scroll;
        let (halign, valign) = match position {
            ToolbarPosition::Bottom => (gtk::Align::Center, gtk::Align::End),
            ToolbarPosition::Top => (gtk::Align::Center, gtk::Align::Start),
            ToolbarPosition::Left => (gtk::Align::Start, gtk::Align::Center),
            ToolbarPosition::Right => (gtk::Align::End, gtk::Align::Center),
        };
        scroll.set_halign(halign);
        scroll.set_valign(valign);
        // 18 px off its own edge, at least 12 px off the two beside it.
        let margin = |edge| if edge == position { 18 } else { 12 };
        scroll.set_margin_bottom(margin(ToolbarPosition::Bottom));
        scroll.set_margin_top(margin(ToolbarPosition::Top));
        scroll.set_margin_start(margin(ToolbarPosition::Left));
        scroll.set_margin_end(margin(ToolbarPosition::Right));
        // Only along the bar: it keeps its natural length when there is room, else scrolls.
        let (along, across) = (gtk::PolicyType::Automatic, gtk::PolicyType::Never);
        if vertical {
            scroll.set_policy(across, along);
        } else {
            scroll.set_policy(along, across);
        }
        scroll.set_propagate_natural_width(!vertical);
        scroll.set_propagate_natural_height(vertical);

        let (orientation, across) = if vertical {
            (gtk::Orientation::Vertical, gtk::Orientation::Horizontal)
        } else {
            (gtk::Orientation::Horizontal, gtk::Orientation::Vertical)
        };
        imp.toolbar.set_orientation(orientation);
        let mut child = imp.toolbar.first_child();
        while let Some(c) = child {
            if let Some(sep) = c.downcast_ref::<gtk::Separator>() {
                sep.set_orientation(across);
            } else if let Some(o) = c.dynamic_cast_ref::<gtk::Orientable>() {
                o.set_orientation(orientation);
            }
            child = c.next_sibling();
        }
    }

    /// Creates the colour swatches and size slots, as many as the preferences ask for.
    pub(super) fn rebuild_slots(&self) {
        let imp = self.imp();
        let (colors, sizes) = settings::with(|s| (s.color_slots, s.size_slots));
        for (button, _) in imp.swatches.take().into_iter().chain(imp.sizes.take()) {
            if let Some(parent) = button.parent().and_downcast::<gtk::Box>() {
                parent.remove(&button);
            }
        }
        let mut swatches = Vec::with_capacity(colors);
        let mut group: Option<gtk::ToggleButton> = None;
        for slot in 0..colors {
            let (button, swatch) = self.slot_button(&mut group, Swatch::new(gdk::RGBA::BLACK));
            button.connect_toggled(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |b| {
                    if b.is_active()
                        && let Some(color) = win.palette().get(slot).copied()
                    {
                        win.set_color(color);
                    }
                }
            ));
            on_retap(
                &button,
                glib::clone!(
                    #[weak(rename_to = win)]
                    self,
                    move |b| win.show_palette(slot, b)
                ),
            );
            imp.swatch_box.append(&button);
            swatches.push((button, swatch));
        }
        let mut group: Option<gtk::ToggleButton> = None;
        let mut slots = Vec::with_capacity(sizes);
        for slot in 0..sizes {
            let (button, dot) = self.slot_button(&mut group, Swatch::new(gdk::RGBA::BLACK));
            button.connect_toggled(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |b| {
                    if b.is_active()
                        && let Some(width) = win.widths().get(slot).copied()
                    {
                        win.set_width(width);
                    }
                }
            ));
            on_retap(
                &button,
                glib::clone!(
                    #[weak(rename_to = win)]
                    self,
                    move |b| win.show_width(slot, b)
                ),
            );
            imp.size_box.append(&button);
            slots.push((button, dot));
        }
        *imp.swatches.borrow_mut() = swatches;
        *imp.sizes.borrow_mut() = slots;
        self.update_toolbar();
    }

    /// A toggle button for one slot of a tool bar group.
    fn slot_button(
        &self,
        group: &mut Option<gtk::ToggleButton>,
        dot: Swatch,
    ) -> (gtk::ToggleButton, Swatch) {
        let button = gtk::ToggleButton::builder()
            .child(&dot)
            .css_classes(["flat"])
            .build();
        button.set_group(group.as_ref());
        group.get_or_insert_with(|| button.clone());
        (button, dot)
    }

    /// The selected preset's id, unless the lasso is selected.
    fn active_preset(&self) -> Option<PresetId> {
        self.imp().tool_state.tool.get().preset()
    }

    /// The selected preset's shown swatch colours (none for the lasso).
    pub(super) fn palette(&self) -> Vec<Rgba8> {
        let Some(preset) = self.imp().tool_state.preset() else {
            return Vec::new();
        };
        let n = settings::with(|s| s.color_slots);
        preset.colors[..n].to_vec()
    }

    /// The selected preset's shown size slots (none for the lasso).
    fn widths(&self) -> Vec<f32> {
        let Some(preset) = self.imp().tool_state.preset() else {
            return Vec::new();
        };
        let n = settings::with(|s| s.size_slots);
        preset.widths[..n].to_vec()
    }

    fn set_width(&self, width: f32) {
        let Some(id) = self.active_preset() else {
            return;
        };
        settings::update(|s| {
            if let Some(p) = s.preset_mut(id) {
                p.width = width;
            }
        });
    }

    fn set_color(&self, color: Rgba8) {
        let Some(id) = self.active_preset() else {
            return;
        };
        settings::update(|s| {
            if let Some(p) = s.preset_mut(id) {
                p.color = color;
            }
        });
        self.update_size_dots();
        self.update_tool_dots();
    }

    /// Swatch colours, size dots and their states for the current tool.
    pub(super) fn update_toolbar(&self) {
        let imp = self.imp();
        let preset = imp.tool_state.preset();
        let role = preset.as_ref().map(|p| p.role);
        let coloured = role.is_some_and(|r| !r.is_eraser());
        imp.swatch_box.set_visible(coloured);
        // The lasso has neither swatches nor sizes: one separator is enough.
        imp.swatch_separator.set_visible(preset.is_some());
        let palette = self.palette();
        for (i, (button, swatch)) in imp.swatches.borrow().iter().enumerate() {
            let Some(&color) = palette.get(i) else { break };
            swatch.set_color(rgba(color));
            button.set_tooltip_text(Some("Colour (tap again for more)"));
            button.set_active(preset.as_ref().is_some_and(|p| p.color == color));
        }
        // Without swatches the separator before them already divides the sizes off.
        imp.size_separator.set_visible(coloured);
        imp.size_box.set_visible(preset.is_some());
        let (Some(preset), Some(role)) = (preset, role) else {
            return;
        };
        let widths = self.widths();
        let unit = if role.is_eraser() { "px" } else { "pt" };
        for (i, (button, dot)) in imp.sizes.borrow().iter().enumerate() {
            let Some(&width) = widths.get(i) else { break };
            dot.set_color(dot_color(role, preset.color));
            dot.set_size(dot_size(role, width));
            button.set_tooltip_text(Some(&format!(
                "{} {unit} (tap again to change)",
                fmt_pt(width)
            )));
            button.set_active((width - preset.width).abs() < 1e-3);
        }
    }

    /// The size slots' dots take the current colour.
    fn update_size_dots(&self) {
        let Some(preset) = self.imp().tool_state.preset() else {
            return;
        };
        let color = dot_color(preset.role, preset.color);
        for (_, dot) in self.imp().sizes.borrow().iter() {
            dot.set_color(color);
        }
    }

    /// The tool buttons' dots take their presets' current colours.
    fn update_tool_dots(&self) {
        for (id, dot) in self.imp().tool_dots.borrow().iter() {
            if let Some(color) = settings::with(|s| s.preset(*id).map(|p| p.color)) {
                dot.set_color(rgba(color));
            }
        }
    }

    /// The preset's options, or the shape for the lasso.
    fn show_tool_popover(&self) {
        match self.imp().tool_state.tool.get() {
            ToolKind::Lasso => self.show_select_shape(),
            ToolKind::Preset(id) => {
                let content = self.preset_popover(id);
                self.tool_popup(&content);
            }
        }
    }

    /// A popover pointing at the tapped tool. It hangs off the tool bar: AdwToggleGroup
    /// hides children it doesn't know about when focus moves into them.
    fn tool_popup(&self, content: &impl IsA<gtk::Widget>) -> gtk::Popover {
        let imp = self.imp();
        let popover = popup(content, &*imp.toolbar);
        // The group's own button under the tap, else the whole group.
        let (x, y) = imp.tool_tap.get();
        let mut button = imp.tools.pick(x, y, gtk::PickFlags::DEFAULT);
        while let Some(w) = button.clone() {
            match w.parent() {
                Some(p) if p == *imp.tools.upcast_ref::<gtk::Widget>() => break,
                p => button = p,
            }
        }
        let target: gtk::Widget = button.unwrap_or_else(|| imp.tools.clone().upcast());
        if let Some(b) = target.compute_bounds(&*imp.toolbar) {
            popover.set_pointing_to(Some(&gdk::Rectangle::new(
                b.x() as i32,
                b.y() as i32,
                b.width() as i32,
                b.height() as i32,
            )));
        }
        popover
    }

    /// Free loop, rectangle, stroke or add space.
    fn show_select_shape(&self) {
        let content = popover_box(gtk::Orientation::Horizontal);
        content.set_spacing(6);
        let current = self.imp().tool_state.select_shape.get();
        let mut group: Option<gtk::ToggleButton> = None;
        for shape in [
            SelectShape::Lasso,
            SelectShape::Rectangle,
            SelectShape::Stroke,
            SelectShape::Space,
        ] {
            let (icon, label, _) = select_shape_info(shape);
            let button = gtk::ToggleButton::builder()
                .child(
                    &adw::ButtonContent::builder()
                        .icon_name(icon)
                        .label(label)
                        .use_underline(true)
                        .build(),
                )
                .css_classes(["flat"])
                .active(current == shape)
                .build();
            button.set_group(group.as_ref());
            group.get_or_insert_with(|| button.clone());
            button.connect_toggled(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |b| {
                    if b.is_active() {
                        win.imp().tool_state.select_shape.set(shape);
                        settings::update(|s| s.select_shape = shape);
                        win.update_lasso_icon();
                    }
                }
            ));
            content.append(&button);
        }
        self.tool_popup(&content);
    }

    /// The lasso's tool button shows the selection shape.
    fn update_lasso_icon(&self) {
        let (icon, _, tooltip) = select_shape_info(self.imp().tool_state.select_shape.get());
        if let Some(toggle) = self.imp().tools.toggle_by_name("lasso") {
            toggle.set_icon_name(Some(icon));
            toggle.set_tooltip(tooltip);
        }
    }

    /// A slider for the width of size slot `slot`; moving it selects the slot.
    fn show_width(&self, slot: usize, anchor: &gtk::ToggleButton) {
        let Some(preset) = self.imp().tool_state.preset() else {
            return;
        };
        let (id, role) = (preset.id, preset.role);
        let (min, max) = role.width_range();
        let fine = role == Role::Ink;
        let step = if fine { 0.1 } else { 1.0 };
        let scale = gtk::Scale::with_range(
            gtk::Orientation::Horizontal,
            f64::from(min),
            f64::from(max),
            step,
        );
        scale.set_value(f64::from(preset.widths[slot]));
        scale.set_digits(if fine { 1 } else { 0 });
        scale.set_draw_value(true);
        scale.set_value_pos(gtk::PositionType::Right);
        scale.set_width_request(240);
        scale.set_hexpand(true);
        let content = popover_box(gtk::Orientation::Horizontal);
        content.append(&gtk::Label::new(Some(if role.is_eraser() {
            "Size"
        } else {
            "Width"
        })));
        content.append(&scale);
        popup(&content, anchor);
        anchor.set_active(true);
        // The settings file is written once the slider rests.
        scale.connect_value_changed(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |s| {
                let w = s.value() as f32;
                settings::change(|st| {
                    if let Some(p) = st.preset_mut(id) {
                        p.widths[slot] = w;
                        p.width = w;
                    }
                });
                let imp = win.imp();
                if let Some((_, dot)) = imp.sizes.borrow().get(slot) {
                    dot.set_size(dot_size(role, w));
                }
            }
        ));
    }

    /// More colours for swatch `slot`: the palette, recent colours and a colour wheel.
    fn show_palette(&self, slot: usize, anchor: &gtk::ToggleButton) {
        let Some(preset) = self.imp().tool_state.preset() else {
            return;
        };
        let presets: &[Rgba8] = if matches!(preset.role, Role::Highlighter) {
            &HIGHLIGHTER_COLORS
        } else {
            &PEN_PRESETS
        };
        let id = preset.id;
        let picker = color_picker(
            presets,
            preset.colors[slot],
            true,
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |color| win.set_palette_slot(id, slot, color)
            ),
            // Only the tool bar's swatch follows the wheel while it is dragged.
            glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |color| {
                    if let Some((_, swatch)) = win.imp().swatches.borrow().get(slot) {
                        swatch.set_color(rgba(color));
                    }
                }
            ),
        );
        let content = popover_box(gtk::Orientation::Vertical);
        content.append(&picker.swatches);
        content.append(&picker.custom);
        popup(&content, anchor);
    }

    fn set_palette_slot(&self, id: PresetId, slot: usize, color: Rgba8) {
        settings::update(|s| {
            if let Some(p) = s.preset_mut(id) {
                p.colors[slot] = color;
                p.color = color;
            }
        });
        self.update_toolbar();
        self.update_tool_dots();
    }
}

impl CeraWindow {
    /// The canvas's context menu at the press `at` (canvas coordinates), with any tool: cut,
    /// copy, delete and arrange on the selection; paste, at `at` when it is a `point` away
    /// from the selection; undo and redo. For the mouse it is a compact menu below the
    /// pointer; for the stylus and fingers it sits above the press, out from under the
    /// hand, with taller rows.
    pub(super) fn show_context_menu(
        &self,
        canvas: &crate::canvas::Canvas,
        at: (f64, f64),
        source: InputSource,
        point: bool,
    ) {
        let selection = canvas.has_selection() && !point;
        self.update_note_actions();
        let menu = gio::Menu::new();
        let edit = gio::Menu::new();
        if selection {
            edit.append(Some("Cu_t"), Some("win.cut"));
            edit.append(Some("_Copy"), Some("win.copy"));
        }
        // Paste is always listed, greyed out with nothing to paste.
        if point {
            let item = gio::MenuItem::new(Some("_Paste"), None);
            item.set_action_and_target_value(Some("win.paste-at"), Some(&at.to_variant()));
            edit.append_item(&item);
        } else {
            edit.append(Some("_Paste"), Some("win.paste"));
        }
        if selection {
            edit.append(Some("_Delete"), Some("win.delete-selection"));
        }
        menu.append_section(None, &edit);
        if selection {
            let style = gio::Menu::new();
            let item = gio::MenuItem::new(Some("Change _Style…"), None);
            item.set_action_and_target_value(
                Some("win.change-style"),
                Some(&(at.0, at.1, source as u32).to_variant()),
            );
            style.append_item(&item);
            menu.append_section(None, &style);
            let arrange = gio::Menu::new();
            for (label, target) in [
                ("Bring to _Front", "front"),
                ("Bring For_ward", "forward"),
                ("Send Back_ward", "backward"),
                ("Send to _Back", "back"),
            ] {
                let item = gio::MenuItem::new(Some(label), None);
                item.set_action_and_target_value(Some("win.arrange"), Some(&target.to_variant()));
                arrange.append_item(&item);
            }
            menu.append_section(None, &arrange);
        }
        let history = gio::Menu::new();
        history.append(Some("_Undo"), Some("win.undo"));
        history.append(Some("_Redo"), Some("win.redo"));
        menu.append_section(None, &history);

        let imp = self.imp();
        let anchor = &*imp.note_overlay;
        let (x, y) = at;
        let Some(origin) =
            canvas.compute_point(anchor, &gtk::graphene::Point::new(x as f32, y as f32))
        else {
            return;
        };
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        if source == InputSource::Mouse {
            popover.set_position(gtk::PositionType::Bottom);
            popover.set_has_arrow(false);
            popover.set_halign(gtk::Align::Start);
        } else {
            popover.set_position(gtk::PositionType::Top);
            popover.add_css_class("touch-menu");
        }
        popover.set_parent(anchor);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(
            origin.x() as i32,
            origin.y() as i32,
            1,
            1,
        )));
        touch_scroll(&popover);
        popover.connect_closed(|m| {
            let m = m.clone();
            glib::idle_add_local_once(move || m.unparent());
        });
        popover.popup();
    }
}

impl CeraWindow {
    /// The selection's style in a popover at the press `at` that opened the context menu:
    /// the presets (each gives the selection its colour, opacity, width, tip and layer), a
    /// palette and colour wheel (each stroke keeps its opacity), and sliders for width, tip
    /// and, when the selection is all highlights, opacity.
    pub(super) fn show_style_popover(&self, at: (f64, f64), source: InputSource) {
        let Some(canvas) = self.canvas() else { return };
        let Some(current) = canvas.selection_style() else {
            return;
        };
        let content = popover_box(gtk::Orientation::Vertical);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .propagate_natural_width(true)
            .max_content_height(520)
            .child(&content)
            .build();

        let anchor = &*self.imp().note_overlay;
        let Some(origin) =
            canvas.compute_point(anchor, &gtk::graphene::Point::new(at.0 as f32, at.1 as f32))
        else {
            return;
        };
        let popover = gtk::Popover::builder()
            .child(&scroll)
            .position(if source == InputSource::Mouse {
                gtk::PositionType::Bottom
            } else {
                gtk::PositionType::Top
            })
            .build();
        popover.set_parent(anchor);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(
            origin.x() as i32,
            origin.y() as i32,
            1,
            1,
        )));
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });

        // Presets: everything but erasers.
        let presets = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .column_spacing(6)
            .row_spacing(6)
            .max_children_per_line(3)
            .homogeneous(true)
            .build();
        let pens: Vec<Preset> = settings::with(|s| {
            s.presets
                .iter()
                .filter(|p| !p.role.is_eraser())
                .cloned()
                .collect()
        });
        for preset in pens {
            let swatch = Swatch::new(rgba(preset.color));
            swatch.set_size(14);
            let label = gtk::Label::builder()
                .label(&preset.name)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            let inner = gtk::Box::builder().spacing(8).build();
            inner.append(&gtk::Image::from_icon_name(preset.role.icon()));
            inner.append(&label);
            inner.append(&swatch);
            let button = gtk::Button::builder()
                .child(&inner)
                .tooltip_text(format!("Draw like {}", preset.name))
                .build();
            button.connect_clicked(glib::clone!(
                #[weak]
                canvas,
                #[weak]
                popover,
                move |_| {
                    let (style, color) = (preset.stroke_style(), preset.ink_color());
                    canvas.restyle_selection(|st| {
                        st.restyled(style, color, preset.width, !preset.pressure)
                    });
                    popover.popdown();
                }
            ));
            presets.append(&button);
        }
        content.append(&presets);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        // Colour, taken when the finger lifts from the wheel, so each pick is one undo step
        // and the selection's texture is redrawn once, not on every motion.
        let palette: &[Rgba8] = if current.highlights {
            &HIGHLIGHTER_COLORS
        } else {
            &PEN_PRESETS
        };
        let picker = color_picker(
            palette,
            current.color,
            false,
            glib::clone!(
                #[weak]
                canvas,
                move |color| canvas.recolour_selection(color)
            ),
            |_| {},
        );
        content.append(&picker.swatches);

        // Type, then width, tip and opacity, each slider applied once it rests.
        let grid = gtk::Grid::builder()
            .column_spacing(12)
            .row_spacing(6)
            .build();
        let kind = adw::ToggleGroup::builder().hexpand(true).build();
        for (name, label) in [("ink", "Ink"), ("highlighter", "Highlighter")] {
            kind.add(adw::Toggle::builder().name(name).label(label).build());
        }
        // A mixed selection starts with neither.
        if current.ink {
            kind.set_active_name(Some("ink"));
        } else if current.highlights {
            kind.set_active_name(Some("highlighter"));
        }
        grid.attach(
            &gtk::Label::builder().label("Type").xalign(0.0).build(),
            0,
            0,
            1,
            1,
        );
        grid.attach(&kind, 1, 0, 1, 1);
        let (mut min, mut max) = if current.highlights {
            Role::Highlighter.width_range()
        } else {
            Role::Ink.width_range()
        };
        (min, max) = (min.min(current.width), max.max(current.width));
        let width = style_slider(&grid, 1, "Width", (min, max), 0.1, current.width);
        width.set_format_value_func(|_, v| format!("{} pt", fmt_pt(v as f32)));
        // The first stroke takes the slider's width and the others keep their proportion to
        // it, so a highlight and the ink beside it both grow.
        let first = current.width.max(0.01);
        on_rest(&width, &canvas, move |canvas, w| {
            let k = w / first;
            canvas
                .restyle_selection(|st| st.restyled(st.style(), st.color(), st.width() * k, false));
        });
        let tip = style_slider(&grid, 2, "Tip", (0.0, 1.0), 0.05, current.roundness);
        tip.set_draw_value(false);
        tip.add_mark(0.0, gtk::PositionType::Bottom, Some("Square"));
        tip.add_mark(1.0, gtk::PositionType::Bottom, Some("Round"));
        on_rest(&tip, &canvas, |canvas, r| {
            let roundness = (r.clamp(0.0, 1.0) * 255.0).round() as u8;
            canvas.restyle_selection(|st| {
                let style = StrokeStyle {
                    roundness,
                    ..st.style()
                };
                st.restyled(style, st.color(), st.width(), false)
            });
        });
        let alpha = f32::from(current.color.a) / 255.0;
        let opacity = style_slider(&grid, 3, "Opacity", (0.05, 1.0), 0.05, alpha);
        opacity.set_format_value_func(|_, v| format!("{:.0} %", v * 100.0));
        let opacity_label = grid.child_at(0, 3);
        // Ink is always opaque.
        let slider = opacity.clone();
        let set_translucent = move |on: bool| {
            slider.set_sensitive(on);
            if let Some(label) = &opacity_label {
                label.set_sensitive(on);
            }
        };
        set_translucent(current.highlights);
        let opacity_changed = on_rest(&opacity, &canvas, |canvas, o| {
            let a = (o.clamp(0.0, 1.0) * 255.0).round() as u8;
            canvas.restyle_selection(|st| st.with_color(Rgba8 { a, ..st.color() }));
        });
        // Ink becomes opaque; a highlight drawn over the ink is translucent, so an opaque
        // stroke takes the highlighter's default opacity rather than hide what is under it.
        kind.connect_active_name_notify(glib::clone!(
            #[weak]
            canvas,
            move |group| {
                let highlighter = match group.active_name().as_deref() {
                    Some("highlighter") => true,
                    Some("ink") => false,
                    _ => return,
                };
                let (layer, default_alpha) = if highlighter {
                    (Layer::Over, HIGHLIGHTER_ALPHA)
                } else {
                    (Layer::Ink, 255)
                };
                canvas.restyle_selection(|st| {
                    let style = StrokeStyle {
                        layer,
                        ..st.style()
                    };
                    let a = match (highlighter, st.color().a) {
                        (true, 255) => default_alpha,
                        (true, a) => a,
                        (false, _) => 255,
                    };
                    st.restyled(style, Rgba8 { a, ..st.color() }, st.width(), false)
                });
                set_translucent(highlighter);
                // Show the new opacity without applying it again.
                let shown = canvas.selection_style().map(|s| s.color.a);
                if let Some(a) = shown {
                    opacity.block_signal(&opacity_changed);
                    opacity.set_value(f64::from(a) / 255.0);
                    opacity.unblock_signal(&opacity_changed);
                }
            }
        ));
        // The sliders sit beside the wheel, which keeps the popover short enough for a
        // landscape tablet.
        let row = gtk::Box::builder().spacing(18).build();
        grid.set_valign(gtk::Align::Center);
        grid.set_width_request(260);
        row.append(&picker.custom);
        row.append(&grid);
        content.append(&row);
        popover.popup();
    }
}

/// A labelled slider in row `row` of a Change style grid.
fn style_slider(
    grid: &gtk::Grid,
    row: i32,
    title: &str,
    (min, max): (f32, f32),
    step: f64,
    value: f32,
) -> gtk::Scale {
    let label = gtk::Label::builder().label(title).xalign(0.0).build();
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min.into(), max.into(), step);
    scale.set_value(value.into());
    scale.set_round_digits(2);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.set_hexpand(true);
    grid.attach(&label, 0, row, 1, 1);
    grid.attach(&scale, 1, row, 1, 1);
    scale
}

/// Runs `f` with the slider's value once it has rested for a quarter second, so a drag is
/// one undo step and one redraw of the selection rather than one per motion.
fn on_rest(
    scale: &gtk::Scale,
    canvas: &crate::canvas::Canvas,
    f: impl Fn(&crate::canvas::Canvas, f32) + 'static,
) -> glib::SignalHandlerId {
    let pending = Rc::new(std::cell::RefCell::new(None::<glib::SourceId>));
    let f = Rc::new(f);
    let canvas = canvas.downgrade();
    scale.connect_value_changed(move |s| {
        if let Some(id) = pending.borrow_mut().take() {
            id.remove();
        }
        let value = s.value() as f32;
        let (f, canvas, again) = (f.clone(), canvas.clone(), pending.clone());
        let id = glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
            again.borrow_mut().take();
            if let Some(canvas) = canvas.upgrade() {
                f(&canvas, value);
            }
        });
        *pending.borrow_mut() = Some(id);
    })
}

/// Lets a finger scroll the menu with GTK's own kinetic scrolling, starting on an item too.
/// A menu item (`GtkModelButton`) claims a touch as soon as it is pressed, which cancels the
/// scrolled window's drag; so each item's click gesture is turned off and replaced by one that
/// activates on release and claims nothing. The scrolled window's drag then claims a touch
/// that moves, which cancels the click.
pub(super) fn touch_scroll(popover: &gtk::PopoverMenu) {
    let mut items = Vec::new();
    menu_items(popover.upcast_ref(), &mut items);
    for item in items {
        let controllers = item.observe_controllers();
        for c in (0..controllers.n_items()).filter_map(|i| controllers.item(i)) {
            if let Ok(click) = c.downcast::<gtk::GestureClick>() {
                click.set_propagation_phase(gtk::PropagationPhase::None);
            }
        }
        let click = gtk::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        click.connect_released(|g, _, x, y| {
            if let Some(item) = g.widget()
                && item.contains(x, y)
            {
                // A model button's activate signal is "clicked": runs and closes the menu.
                item.activate();
            }
        });
        item.add_controller(click);
    }
}

/// The menu items (`GtkModelButton`s) inside `widget`.
fn menu_items(widget: &gtk::Widget, out: &mut Vec<gtk::Widget>) {
    let mut child = widget.first_child();
    while let Some(c) = child {
        if c.css_name() == "modelbutton" {
            out.push(c.clone());
        } else {
            menu_items(&c, out);
        }
        child = c.next_sibling();
    }
}

/// Runs `f` when the button is tapped while already selected. The capture phase sees the
/// press before the button handles it; `f` runs after the release, once the tap is done.
fn on_retap(button: &gtk::ToggleButton, f: impl Fn(&gtk::ToggleButton) + 'static) {
    let tap = gtk::GestureClick::new();
    tap.set_propagation_phase(gtk::PropagationPhase::Capture);
    let was_active = Rc::new(Cell::new(false));
    tap.connect_pressed(glib::clone!(
        #[weak]
        button,
        #[strong]
        was_active,
        move |_, _, _, _| was_active.set(button.is_active())
    ));
    let f = Rc::new(f);
    tap.connect_released(glib::clone!(
        #[weak]
        button,
        move |_, _, _, _| {
            if was_active.take() {
                let f = f.clone();
                glib::idle_add_local_once(glib::clone!(
                    #[weak]
                    button,
                    move || f(&button)
                ));
            }
        }
    ));
    button.add_controller(tap);
}

/// Size dots show the preset's colour; an eraser's are grey.
fn dot_color(role: Role, current: Rgba8) -> gdk::RGBA {
    if role.is_eraser() {
        gdk::RGBA::new(0.55, 0.55, 0.55, 1.0)
    } else {
        rgba(current)
    }
}

/// A size slot's dot: on a log scale over the role's range, so fine widths stay apart.
fn dot_size(role: Role, width: f32) -> i32 {
    let (min, max) = role.width_range();
    let t = ((width / min).ln() / (max / min).ln()).clamp(0.0, 1.0);
    (DOT_RANGE.0 + t * (DOT_RANGE.1 - DOT_RANGE.0)).round() as i32
}

/// "1.6", "12": a width without trailing zeros.
fn fmt_pt(width: f32) -> String {
    let s = format!("{width:.1}");
    s.strip_suffix(".0").map_or(s.clone(), str::to_owned)
}

pub(super) fn popover_box(orientation: gtk::Orientation) -> gtk::Box {
    gtk::Box::builder()
        .orientation(orientation)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build()
}

/// A popover from the tool bar, opening away from the window edge the bar sits on; removed
/// once closed.
pub(super) fn popup(
    content: &impl IsA<gtk::Widget>,
    anchor: &impl IsA<gtk::Widget>,
) -> gtk::Popover {
    let side = match settings::with(|s| s.toolbar_position) {
        ToolbarPosition::Bottom => gtk::PositionType::Top,
        ToolbarPosition::Top => gtk::PositionType::Bottom,
        ToolbarPosition::Left => gtk::PositionType::Right,
        ToolbarPosition::Right => gtk::PositionType::Left,
    };
    let popover = gtk::Popover::builder()
        .child(content)
        .position(side)
        .build();
    popover.set_parent(anchor);
    popover.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
    popover.popup();
    popover
}

/// Icon, popover label and tool bar tooltip for a selection shape.
fn select_shape_info(shape: SelectShape) -> (&'static str, &'static str, &'static str) {
    match shape {
        SelectShape::Lasso => ("cera-lasso-symbolic", "_Lasso", "Lasso (L)"),
        SelectShape::Rectangle => (
            "cera-select-rect-symbolic",
            "_Rectangle",
            "Rectangle select (L)",
        ),
        SelectShape::Stroke => (
            "cera-select-stroke-symbolic",
            "_Stroke",
            "Stroke select (L)",
        ),
        SelectShape::Space => ("cera-add-space-symbolic", "_Add Space", "Add space (L)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dots_grow_with_width() {
        let pen = [0.2, 0.8, 1.6, 3.2, 10.0].map(|w| dot_size(Role::Ink, w));
        assert!(pen.windows(2).all(|p| p[0] < p[1]), "{pen:?}");
        assert_eq!(pen[0], DOT_RANGE.0 as i32);
        assert_eq!(pen[4], DOT_RANGE.1 as i32);
    }

    #[test]
    fn widths_print_short() {
        assert_eq!(fmt_pt(1.6), "1.6");
        assert_eq!(fmt_pt(12.0), "12");
    }
}
