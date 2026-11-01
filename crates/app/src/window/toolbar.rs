// SPDX-License-Identifier: GPL-3.0-or-later
//! The floating tool bar (DESIGN.md §9): tools, colour swatches, size slots, undo and redo
//! (the lasso's actions are in the canvas's context menu, `show_context_menu`). Which window
//! edge it sits on and how many swatches and size slots it shows are preferences; the
//! eraser has size slots but no swatches, the lasso neither (Change style in the context
//! menu recolours a selection). Tapping the active pen again opens its pressure
//! setting, the active lasso its shape (free loop or rectangle); tapping the selected swatch
//! again opens the palette and colour wheel for that slot, the selected size slot a slider.
//! Tool settings and slots are saved in the app settings.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use cera_core::Rgba8;
use gtk::{gdk, gio, glib};

use super::{CeraWindow, rgba};
use crate::canvas::{
    ERASER_WIDTH_RANGE, HIGHLIGHTER_ALPHA, HIGHLIGHTER_COLORS, HIGHLIGHTER_WIDTH_RANGE,
    InputSource, PEN_WIDTH_RANGE, ToolKind,
};
use crate::settings::{self, SelectShape, ToolbarPosition};
use crate::swatch::Swatch;
use crate::wheel::ColorWheel;

/// Palette choices: GNOME's palette for the pen, translucent tints for the highlighter.
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

impl CeraWindow {
    pub(super) fn setup_toolbar(&self) {
        let imp = self.imp();
        let tools = &imp.tool_state;
        settings::with(|s| {
            tools.pen_color.set(s.pen_colors[0]);
            tools.highlighter_color.set(s.highlighter_colors[0]);
            tools.pen_width.set(s.pen_width);
            tools.highlighter_width.set(s.highlighter_width);
            tools.pressure.set(s.pressure);
            tools.select_shape.set(s.select_shape);
        });
        self.update_lasso_icon();
        imp.tools.connect_active_name_notify(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |group| {
                let tool = group
                    .active_name()
                    .as_deref()
                    .and_then(ToolKind::from_name)
                    .unwrap_or(ToolKind::Pen);
                win.imp().tool_state.tool.set(tool);
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
        self.place_toolbar();
        self.rebuild_slots();
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
                    let tools = &win.imp().tool_state;
                    if b.is_active() {
                        tools.set_color(win.palette()[slot]);
                        win.update_size_dots();
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
                    if b.is_active() {
                        win.set_width(win.widths()[slot]);
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

    /// The swatches' colours: the highlighter's for the highlighter, the pen's otherwise.
    pub(super) fn palette(&self) -> Vec<Rgba8> {
        settings::with(|s| match self.imp().tool_state.tool.get() {
            ToolKind::Highlighter => s.highlighter_colors[..s.color_slots].to_vec(),
            _ => s.pen_colors[..s.color_slots].to_vec(),
        })
    }

    /// The size slots' widths for the current tool, the pen's for the lasso.
    fn widths(&self) -> Vec<f32> {
        settings::with(|s| match self.imp().tool_state.tool.get() {
            ToolKind::Highlighter => s.highlighter_widths[..s.size_slots].to_vec(),
            ToolKind::Eraser => s.eraser_widths[..s.size_slots].to_vec(),
            _ => s.pen_widths[..s.size_slots].to_vec(),
        })
    }

    fn set_width(&self, width: f32) {
        let tools = &self.imp().tool_state;
        let tool = tools.tool.get();
        tools.set_width(width);
        settings::update(|s| match tool {
            ToolKind::Highlighter => s.highlighter_width = width,
            ToolKind::Pen => s.pen_width = width,
            ToolKind::Eraser => s.eraser_width = width,
            ToolKind::Lasso => {}
        });
    }

    /// Swatch colours, size dots and their states, and the lasso's buttons, for the current
    /// tool.
    pub(super) fn update_toolbar(&self) {
        let imp = self.imp();
        let tool = imp.tool_state.tool.get();
        let palette = self.palette();
        let current = imp.tool_state.color();
        let coloured = matches!(tool, ToolKind::Pen | ToolKind::Highlighter);
        imp.swatch_box.set_visible(coloured);
        // The lasso has neither swatches nor sizes: one separator is enough.
        imp.swatch_separator.set_visible(tool != ToolKind::Lasso);
        for (i, (button, swatch)) in imp.swatches.borrow().iter().enumerate() {
            swatch.set_color(rgba(palette[i]));
            button.set_tooltip_text(Some("Colour (tap again for more)"));
            button.set_active(palette[i] == current);
        }
        let sized = tool != ToolKind::Lasso;
        // Without swatches the separator before them already divides the sizes off.
        imp.size_separator
            .set_visible(matches!(tool, ToolKind::Pen | ToolKind::Highlighter));
        imp.size_box.set_visible(sized);
        let widths = self.widths();
        let width = imp.tool_state.width();
        let unit = if tool == ToolKind::Eraser { "px" } else { "pt" };
        for (i, (button, dot)) in imp.sizes.borrow().iter().enumerate() {
            dot.set_color(dot_color(tool, current));
            dot.set_size(dot_size(tool, widths[i]));
            button.set_tooltip_text(Some(&format!(
                "{} {unit} (tap again to change)",
                fmt_pt(widths[i])
            )));
            button.set_active((widths[i] - width).abs() < 1e-3);
        }
    }

    /// The size slots' dots take the current colour.
    fn update_size_dots(&self) {
        let tools = &self.imp().tool_state;
        let color = dot_color(tools.tool.get(), tools.color());
        for (_, dot) in self.imp().sizes.borrow().iter() {
            dot.set_color(color);
        }
    }

    /// Pressure for the pen, the shape for the lasso.
    fn show_tool_popover(&self) {
        let imp = self.imp();
        let tools = imp.tool_state.clone();
        match tools.tool.get() {
            ToolKind::Pen => {}
            ToolKind::Lasso => return self.show_select_shape(),
            _ => return,
        }
        let label = gtk::Label::builder()
            .label("Pressure")
            .xalign(0.0)
            .hexpand(true)
            .build();
        let switch = gtk::Switch::builder()
            .active(tools.pressure.get())
            .valign(gtk::Align::Center)
            .build();
        switch.connect_active_notify(move |s| {
            tools.pressure.set(s.is_active());
            settings::update(|st| st.pressure = s.is_active());
        });
        let content = popover_box(gtk::Orientation::Horizontal);
        content.append(&label);
        content.append(&switch);
        self.tool_popup(&content);
    }

    /// A popover pointing at the tapped tool. It hangs off the tool bar: AdwToggleGroup
    /// hides children it doesn't know about when focus moves into them.
    fn tool_popup(&self, content: &impl IsA<gtk::Widget>) {
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
        let tool = self.imp().tool_state.tool.get();
        let (min, max) = match tool {
            ToolKind::Pen => PEN_WIDTH_RANGE,
            ToolKind::Highlighter => HIGHLIGHTER_WIDTH_RANGE,
            ToolKind::Eraser => ERASER_WIDTH_RANGE,
            ToolKind::Lasso => return,
        };
        let step = if tool == ToolKind::Pen { 0.1 } else { 1.0 };
        let scale = gtk::Scale::with_range(
            gtk::Orientation::Horizontal,
            f64::from(min),
            f64::from(max),
            step,
        );
        scale.set_value(f64::from(self.widths()[slot]));
        scale.set_digits(if tool == ToolKind::Pen { 1 } else { 0 });
        scale.set_draw_value(true);
        scale.set_value_pos(gtk::PositionType::Right);
        scale.set_width_request(240);
        scale.set_hexpand(true);
        let content = popover_box(gtk::Orientation::Horizontal);
        content.append(&gtk::Label::new(Some(if tool == ToolKind::Eraser {
            "Size"
        } else {
            "Width"
        })));
        content.append(&scale);
        let popover = popup(&content, anchor);
        anchor.set_active(true);
        // The settings file is written once, when the popover closes.
        let width = Rc::new(Cell::new(None::<f32>));
        scale.connect_value_changed(glib::clone!(
            #[weak(rename_to = win)]
            self,
            #[strong]
            width,
            move |s| {
                let w = s.value() as f32;
                width.set(Some(w));
                let imp = win.imp();
                imp.tool_state.set_width(w);
                if let Some((_, dot)) = imp.sizes.borrow().get(slot) {
                    dot.set_size(dot_size(tool, w));
                }
            }
        ));
        popover.connect_closed(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |_| {
                let Some(w) = width.take() else { return };
                settings::update(|s| match tool {
                    ToolKind::Highlighter => s.highlighter_widths[slot] = w,
                    ToolKind::Eraser => s.eraser_widths[slot] = w,
                    _ => s.pen_widths[slot] = w,
                });
                if win.imp().tool_state.tool.get() == tool {
                    win.set_width(w);
                    win.update_toolbar();
                }
            }
        ));
    }

    /// More colours for swatch `slot`, and a colour wheel.
    fn show_palette(&self, slot: usize, anchor: &gtk::ToggleButton) {
        let highlighter = self.imp().tool_state.tool.get() == ToolKind::Highlighter;
        let presets: &[Rgba8] = if highlighter {
            &HIGHLIGHTER_COLORS
        } else {
            &PEN_PRESETS
        };
        let wrap = adw::WrapBox::builder()
            .child_spacing(6)
            .line_spacing(6)
            .natural_line_length(6 * 46)
            .justify(adw::JustifyMode::Spread)
            .build();
        let content = popover_box(gtk::Orientation::Vertical);
        content.append(&wrap);
        let wheel = ColorWheel::new(self.palette()[slot]);
        content.append(&wheel);
        let popover = popup(&content, anchor);
        for &color in presets {
            let button = gtk::Button::builder()
                .child(&Swatch::new(rgba(color)))
                .css_classes(["flat", "circular"])
                .width_request(40)
                .height_request(40)
                .build();
            button.connect_clicked(glib::clone!(
                #[weak(rename_to = win)]
                self,
                #[weak]
                popover,
                move |_| {
                    win.set_palette_slot(slot, color, highlighter);
                    popover.popdown();
                }
            ));
            wrap.append(&button);
        }
        let alpha = if highlighter { HIGHLIGHTER_ALPHA } else { 255 };
        // While dragging only the swatch follows; the colour is taken when the finger lifts.
        wheel.connect_changed(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |w| {
                if let Some((_, swatch)) = win.imp().swatches.borrow().get(slot) {
                    swatch.set_color(rgba(w.color()));
                }
            }
        ));
        wheel.connect_picked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            move |w| {
                let color = Rgba8 {
                    a: alpha,
                    ..w.color()
                };
                win.set_palette_slot(slot, color, highlighter);
            }
        ));
    }

    fn set_palette_slot(&self, slot: usize, color: Rgba8, highlighter: bool) {
        settings::update(|s| {
            if highlighter {
                s.highlighter_colors[slot] = color;
            } else {
                s.pen_colors[slot] = color;
            }
        });
        self.imp().tool_state.set_color(color);
        self.update_toolbar();
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
    /// The selection's style, for now its colour: presets and a colour wheel at the press
    /// `at` that opened the context menu. Highlighter strokes keep their translucency.
    pub(super) fn show_style_popover(&self, at: (f64, f64), source: InputSource) {
        let Some(canvas) = self.canvas() else { return };
        let Some((current, highlighter)) = canvas.selection_style() else {
            return;
        };
        let presets: &[Rgba8] = if highlighter {
            &HIGHLIGHTER_COLORS
        } else {
            &PEN_PRESETS
        };
        let wrap = adw::WrapBox::builder()
            .child_spacing(6)
            .line_spacing(6)
            .natural_line_length(6 * 46)
            .justify(adw::JustifyMode::Spread)
            .build();
        let content = popover_box(gtk::Orientation::Vertical);
        content.append(&wrap);
        let wheel = ColorWheel::new(Rgba8 { a: 255, ..current });
        content.append(&wheel);

        let anchor = &*self.imp().note_overlay;
        let Some(origin) =
            canvas.compute_point(anchor, &gtk::graphene::Point::new(at.0 as f32, at.1 as f32))
        else {
            return;
        };
        let popover = gtk::Popover::builder()
            .child(&content)
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
        for &color in presets {
            let button = gtk::Button::builder()
                .child(&Swatch::new(rgba(color)))
                .css_classes(["flat", "circular"])
                .width_request(40)
                .height_request(40)
                .build();
            button.connect_clicked(glib::clone!(
                #[weak]
                canvas,
                #[weak]
                popover,
                move |_| {
                    canvas.recolour_selection(color);
                    popover.popdown();
                }
            ));
            wrap.append(&button);
        }
        // Taken when the finger lifts, so each pick is one undo step and the selection's
        // texture is redrawn once, not on every motion.
        wheel.connect_picked(glib::clone!(
            #[weak]
            canvas,
            move |w| canvas.recolour_selection(w.color())
        ));
        popover.popup();
    }
}

/// Lets a finger scroll the menu with GTK's own kinetic scrolling, starting on an item too.
/// A menu item (`GtkModelButton`) claims a touch as soon as it is pressed, which cancels the
/// scrolled window's drag; so each item's click gesture is turned off and replaced by one that
/// activates on release and claims nothing. The scrolled window's drag then claims a touch
/// that moves, which cancels the click.
fn touch_scroll(popover: &gtk::PopoverMenu) {
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

/// Size dots show the tool's colour; the eraser's are grey.
fn dot_color(tool: ToolKind, current: Rgba8) -> gdk::RGBA {
    if tool == ToolKind::Eraser {
        gdk::RGBA::new(0.55, 0.55, 0.55, 1.0)
    } else {
        rgba(current)
    }
}

/// A size slot's dot: on a log scale over the tool's range, so fine widths stay apart.
fn dot_size(tool: ToolKind, width: f32) -> i32 {
    let (min, max) = match tool {
        ToolKind::Highlighter => HIGHLIGHTER_WIDTH_RANGE,
        ToolKind::Eraser => ERASER_WIDTH_RANGE,
        _ => PEN_WIDTH_RANGE,
    };
    let t = ((width / min).ln() / (max / min).ln()).clamp(0.0, 1.0);
    (DOT_RANGE.0 + t * (DOT_RANGE.1 - DOT_RANGE.0)).round() as i32
}

/// "1.6", "12": a width without trailing zeros.
fn fmt_pt(width: f32) -> String {
    let s = format!("{width:.1}");
    s.strip_suffix(".0").map_or(s.clone(), str::to_owned)
}

fn popover_box(orientation: gtk::Orientation) -> gtk::Box {
    gtk::Box::builder()
        .orientation(orientation)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build()
}

/// A popover above `anchor`, removed once closed.
/// A popover from the tool bar, opening away from the window edge the bar sits on.
fn popup(content: &impl IsA<gtk::Widget>, anchor: &impl IsA<gtk::Widget>) -> gtk::Popover {
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
        let pen = [0.2, 0.8, 1.6, 3.2, 10.0].map(|w| dot_size(ToolKind::Pen, w));
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
