// SPDX-License-Identifier: GPL-3.0-or-later
//! The floating tool bar (DESIGN.md §9): tools, colour swatches, size slots, the lasso's
//! actions and "add page". How many swatches and size slots it shows is a preference.
//! Tapping the active pen again opens its pressure setting, the active lasso its shape
//! (free loop or rectangle); a long press (or right click) on
//! a swatch opens the palette and colour wheel for that slot, on a size slot a width slider.
//! Tool settings and slots are saved in the app settings.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use cera_core::Rgba8;
use gtk::{gdk, glib};

use super::{CeraWindow, rgba};
use crate::canvas::{
    HIGHLIGHTER_ALPHA, HIGHLIGHTER_COLORS, HIGHLIGHTER_WIDTH_RANGE, PEN_WIDTH_RANGE, ToolKind,
};
use crate::settings;
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
            tools.rect_select.set(s.rect_select);
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
        self.rebuild_slots();
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
                    if b.is_active() && tools.tool.get() != ToolKind::Lasso {
                        tools.set_color(win.palette()[slot]);
                        win.update_size_dots();
                    }
                }
            ));
            button.connect_clicked(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |_| {
                    if win.imp().tool_state.tool.get() == ToolKind::Lasso
                        && let Some(canvas) = win.canvas()
                    {
                        canvas.recolour_selection(win.palette()[slot]);
                    }
                }
            ));
            on_hold(
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
            on_hold(
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

    /// The size slots' widths for the current tool, the pen's for tools without a width.
    fn widths(&self) -> Vec<f32> {
        settings::with(|s| match self.imp().tool_state.tool.get() {
            ToolKind::Highlighter => s.highlighter_widths[..s.size_slots].to_vec(),
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
            ToolKind::Eraser | ToolKind::Lasso => {}
        });
    }

    /// Swatch colours, size dots and their states, and the lasso's buttons, for the current
    /// tool.
    pub(super) fn update_toolbar(&self) {
        let imp = self.imp();
        let tool = imp.tool_state.tool.get();
        let palette = self.palette();
        let current = imp.tool_state.color();
        let selection = self.canvas().is_some_and(|c| c.has_selection());
        imp.selection_box.set_visible(tool == ToolKind::Lasso);
        for (i, (button, swatch)) in imp.swatches.borrow().iter().enumerate() {
            swatch.set_color(rgba(palette[i]));
            button.set_sensitive(match tool {
                ToolKind::Eraser => false,
                ToolKind::Lasso => selection,
                _ => true,
            });
            button.set_tooltip_text(Some(if tool == ToolKind::Lasso {
                "Recolour the selection"
            } else {
                "Colour (hold for more)"
            }));
            button.set_active(palette[i] == current);
        }
        let draws = matches!(tool, ToolKind::Pen | ToolKind::Highlighter);
        imp.size_separator.set_visible(draws);
        imp.size_box.set_visible(draws);
        let widths = self.widths();
        let width = imp.tool_state.width();
        for (i, (button, dot)) in imp.sizes.borrow().iter().enumerate() {
            dot.set_color(rgba(current));
            dot.set_size(dot_size(tool, widths[i]));
            button.set_tooltip_text(Some(&format!("{} pt (hold to change)", fmt_pt(widths[i]))));
            button.set_active((widths[i] - width).abs() < 1e-3);
        }
    }

    /// The size slots' dots take the current colour.
    fn update_size_dots(&self) {
        let color = rgba(self.imp().tool_state.color());
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

    /// Free loop or rectangle.
    fn show_select_shape(&self) {
        let content = popover_box(gtk::Orientation::Horizontal);
        content.set_spacing(6);
        let rect = self.imp().tool_state.rect_select.get();
        let mut group: Option<gtk::ToggleButton> = None;
        for (shape_rect, icon, label) in [
            (false, "cera-lasso-symbolic", "_Lasso"),
            (true, "cera-select-rect-symbolic", "_Rectangle"),
        ] {
            let button = gtk::ToggleButton::builder()
                .child(
                    &adw::ButtonContent::builder()
                        .icon_name(icon)
                        .label(label)
                        .use_underline(true)
                        .build(),
                )
                .css_classes(["flat"])
                .active(rect == shape_rect)
                .build();
            button.set_group(group.as_ref());
            group.get_or_insert_with(|| button.clone());
            button.connect_toggled(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |b| {
                    if b.is_active() {
                        win.imp().tool_state.rect_select.set(shape_rect);
                        settings::update(|s| s.rect_select = shape_rect);
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
        let rect = self.imp().tool_state.rect_select.get();
        if let Some(toggle) = self.imp().tools.toggle_by_name("lasso") {
            toggle.set_icon_name(Some(if rect {
                "cera-select-rect-symbolic"
            } else {
                "cera-lasso-symbolic"
            }));
            toggle.set_tooltip(if rect {
                "Rectangle select (L)"
            } else {
                "Lasso (L)"
            });
        }
    }

    /// A slider for the width of size slot `slot`; moving it selects the slot.
    fn show_width(&self, slot: usize, anchor: &gtk::ToggleButton) {
        let tool = self.imp().tool_state.tool.get();
        let (min, max) = match tool {
            ToolKind::Pen => PEN_WIDTH_RANGE,
            ToolKind::Highlighter => HIGHLIGHTER_WIDTH_RANGE,
            _ => return,
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
        content.append(&gtk::Label::new(Some("Width")));
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
        // While dragging only the swatch follows; the colour is taken when the finger lifts,
        // so recolouring a selection is one undo step.
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
        let tools = &self.imp().tool_state;
        if tools.tool.get() == ToolKind::Lasso {
            if let Some(canvas) = self.canvas() {
                canvas.recolour_selection(color);
            }
        } else {
            tools.set_color(color);
        }
        self.update_toolbar();
    }
}

/// Runs `f` on a long press or a right click.
fn on_hold(button: &gtk::ToggleButton, f: impl Fn(&gtk::ToggleButton) + Clone + 'static) {
    let hold = gtk::GestureLongPress::new();
    let held = f.clone();
    hold.connect_pressed(glib::clone!(
        #[weak]
        button,
        move |g, _, _| {
            g.set_state(gtk::EventSequenceState::Claimed);
            held(&button);
        }
    ));
    button.add_controller(hold);
    let right = gtk::GestureClick::builder()
        .button(gdk::BUTTON_SECONDARY)
        .build();
    right.connect_pressed(glib::clone!(
        #[weak]
        button,
        move |g, _, _, _| {
            g.set_state(gtk::EventSequenceState::Claimed);
            f(&button);
        }
    ));
    button.add_controller(right);
}

/// A size slot's dot: on a log scale over the tool's range, so fine widths stay apart.
fn dot_size(tool: ToolKind, width: f32) -> i32 {
    let (min, max) = match tool {
        ToolKind::Highlighter => HIGHLIGHTER_WIDTH_RANGE,
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
fn popup(content: &impl IsA<gtk::Widget>, anchor: &impl IsA<gtk::Widget>) -> gtk::Popover {
    let popover = gtk::Popover::builder()
        .child(content)
        .position(gtk::PositionType::Top)
        .build();
    popover.set_parent(anchor);
    popover.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
    popover.popup();
    popover
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
