// SPDX-License-Identifier: GPL-3.0-or-later
//! The floating tool bar (DESIGN.md §9): tools, three colour swatches, the lasso's actions
//! and "add page". Tapping the active pen or highlighter again opens its width and pressure
//! settings; a long press (or right click) on a swatch opens the palette for that slot.
//! Tool settings and palettes are saved in the app settings.

use adw::prelude::*;
use adw::subclass::prelude::*;
use cera_core::Rgba8;
use gtk::{gdk, glib};

use super::{CeraWindow, rgba};
use crate::canvas::{HIGHLIGHTER_WIDTHS, PEN_WIDTHS, ToolKind};
use crate::settings;
use crate::swatch::Swatch;

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
const HIGHLIGHTER_ALPHA: u8 = 90;
const HIGHLIGHTER_PRESETS: [Rgba8; 6] = [
    Rgba8::new(246, 211, 45, HIGHLIGHTER_ALPHA),
    Rgba8::new(87, 227, 137, HIGHLIGHTER_ALPHA),
    Rgba8::new(246, 97, 81, HIGHLIGHTER_ALPHA),
    Rgba8::new(98, 160, 234, HIGHLIGHTER_ALPHA),
    Rgba8::new(220, 138, 221, HIGHLIGHTER_ALPHA),
    Rgba8::new(255, 163, 72, HIGHLIGHTER_ALPHA),
];
/// Dot sizes for the fine, medium and broad width buttons.
const WIDTH_DOTS: [i32; 3] = [6, 11, 18];
const WIDTH_NAMES: [&str; 3] = ["Fine", "Medium", "Broad"];

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
        });
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
        let pressed_on = std::rc::Rc::new(std::cell::RefCell::new(None::<glib::GString>));
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
            move |_, _, _, _| {
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

        let mut first: Option<gtk::ToggleButton> = None;
        for slot in 0..3 {
            let swatch = Swatch::new(gdk::RGBA::BLACK);
            let button = gtk::ToggleButton::builder()
                .child(&swatch)
                .css_classes(["flat"])
                .tooltip_text("Colour (hold for more)")
                .build();
            button.set_group(first.as_ref());
            first.get_or_insert_with(|| button.clone());
            button.connect_toggled(glib::clone!(
                #[weak(rename_to = win)]
                self,
                move |b| {
                    let tools = &win.imp().tool_state;
                    if b.is_active() && tools.tool.get() != ToolKind::Lasso {
                        tools.set_color(win.palette()[slot]);
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
            let hold = gtk::GestureLongPress::new();
            hold.connect_pressed(glib::clone!(
                #[weak(rename_to = win)]
                self,
                #[weak]
                button,
                move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    win.show_palette(slot, &button);
                }
            ));
            button.add_controller(hold);
            let right = gtk::GestureClick::builder()
                .button(gdk::BUTTON_SECONDARY)
                .build();
            right.connect_pressed(glib::clone!(
                #[weak(rename_to = win)]
                self,
                #[weak]
                button,
                move |g, _, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    win.show_palette(slot, &button);
                }
            ));
            button.add_controller(right);
            imp.swatch_box.append(&button);
            imp.swatches.borrow_mut().push((button, swatch));
        }
        self.update_toolbar();
    }

    /// The swatches' colours: the highlighter's for the highlighter, the pen's otherwise.
    pub(super) fn palette(&self) -> [Rgba8; 3] {
        settings::with(|s| match self.imp().tool_state.tool.get() {
            ToolKind::Highlighter => s.highlighter_colors,
            _ => s.pen_colors,
        })
    }

    /// Swatch colours and states, and the lasso's buttons, for the current tool.
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
            if palette[i] == current {
                button.set_active(true);
            }
        }
    }

    /// Width presets, and pressure for the pen.
    fn show_tool_popover(&self) {
        let imp = self.imp();
        let tools = imp.tool_state.clone();
        let tool = tools.tool.get();
        let widths = match tool {
            ToolKind::Pen => PEN_WIDTHS,
            ToolKind::Highlighter => HIGHLIGHTER_WIDTHS,
            _ => return,
        };
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let mut group: Option<gtk::ToggleButton> = None;
        for (i, width) in widths.into_iter().enumerate() {
            let dot = Swatch::new(rgba(tools.color()));
            dot.set_size(WIDTH_DOTS[i]);
            let button = gtk::ToggleButton::builder()
                .child(&dot)
                .css_classes(["flat", "circular"])
                .tooltip_text(WIDTH_NAMES[i])
                .width_request(44)
                .height_request(44)
                .build();
            button.set_group(group.as_ref());
            group.get_or_insert_with(|| button.clone());
            button.set_active((tools.width() - width).abs() < 1e-3);
            let tools = tools.clone();
            button.connect_toggled(move |b| {
                if b.is_active() {
                    tools.set_width(width);
                    settings::update(|s| match tool {
                        ToolKind::Highlighter => s.highlighter_width = width,
                        _ => s.pen_width = width,
                    });
                }
            });
            row.append(&button);
        }
        content.append(&row);
        if tool == ToolKind::Pen {
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
            let pressure = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            pressure.append(&label);
            pressure.append(&switch);
            content.append(&pressure);
        }
        popup(&content, &*imp.tools);
    }

    /// More colours for swatch `slot`, and a custom colour.
    fn show_palette(&self, slot: usize, anchor: &gtk::ToggleButton) {
        let highlighter = self.imp().tool_state.tool.get() == ToolKind::Highlighter;
        let presets: &[Rgba8] = if highlighter {
            &HIGHLIGHTER_PRESETS
        } else {
            &PEN_PRESETS
        };
        let wrap = adw::WrapBox::builder()
            .child_spacing(6)
            .line_spacing(6)
            .natural_line_length(6 * 46)
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        content.append(&wrap);
        let custom = gtk::Button::builder()
            .label("_Custom…")
            .use_underline(true)
            .build();
        content.append(&custom);
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
        custom.connect_clicked(glib::clone!(
            #[weak(rename_to = win)]
            self,
            #[weak]
            popover,
            move |_| {
                popover.popdown();
                let initial = rgba(win.palette()[slot]);
                let dialog = gtk::ColorDialog::builder().with_alpha(false).build();
                glib::spawn_future_local(glib::clone!(
                    #[weak]
                    win,
                    async move {
                        // An error means the dialog was cancelled.
                        let Ok(c) = dialog.choose_rgba_future(Some(&win), Some(&initial)).await
                        else {
                            return;
                        };
                        let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                        let alpha = if highlighter { HIGHLIGHTER_ALPHA } else { 255 };
                        let color =
                            Rgba8::new(byte(c.red()), byte(c.green()), byte(c.blue()), alpha);
                        win.set_palette_slot(slot, color, highlighter);
                    }
                ));
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
